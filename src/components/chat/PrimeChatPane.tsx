import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { Icon } from "@iconify/react";
import { api } from "../../lib/ipc";
import type { PrimeEventPayload, PrimeSessionConfig } from "../../types";
import { Markdown } from "./Markdown";

// Minimal Prime Agent chat: one daemon session per pane (Workspace mounts
// one pane per yusAi conversation), created the first time the pane is
// shown so the model and thinking pickers reflect the worker's state. Only
// user prompts and assistant text are rendered; tool calls, thinking and
// sub-agents are out of scope for this milestone.

type PrimeMessage = {
  id: number;
  role: "user" | "assistant" | "error";
  text: string;
};

type PrimeStatus = "idle" | "starting" | "streaming";

type Props = {
  workspacePath: string;
  // The pane is the one on screen (Prime engine, active conversation).
  active: boolean;
  headerExtra?: ReactNode;
  onOpenFile: (path: string) => void;
};

export function PrimeChatPane({ workspacePath, active, headerExtra, onOpenFile }: Props) {
  const [messages, setMessages] = useState<PrimeMessage[]>([]);
  const [status, setStatus] = useState<PrimeStatus>("idle");
  const [text, setText] = useState("");
  const [config, setConfig] = useState<PrimeSessionConfig | null>(null);
  const [configBusy, setConfigBusy] = useState(false);
  const sessionIdRef = useRef<string | null>(null);
  // The in-flight session creation, shared by the eager start and a send.
  const creatingRef = useRef<Promise<string> | null>(null);
  const unmountedRef = useRef(false);
  const nextIdRef = useRef(1);
  // The assistant message currently receiving text deltas.
  const streamingIdRef = useRef<number | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);

  const pushMessage = useCallback((role: PrimeMessage["role"], value: string) => {
    const id = nextIdRef.current++;
    setMessages((current) => [...current, { id, role, text: value }]);
    return id;
  }, []);

  const appendAssistantText = useCallback(
    (delta: string) => {
      let id = streamingIdRef.current;
      if (id === null) {
        id = pushMessage("assistant", "");
        streamingIdRef.current = id;
      }
      const target = id;
      setMessages((current) =>
        current.map((message) =>
          message.id === target ? { ...message, text: message.text + delta } : message,
        ),
      );
    },
    [pushMessage],
  );

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    void listen<PrimeEventPayload>("prime-event", ({ payload }) => {
      const sessionId = sessionIdRef.current;
      if (payload.kind === "disconnected") {
        if (!sessionId) return;
        sessionIdRef.current = null;
        streamingIdRef.current = null;
        setStatus("idle");
        setConfig(null);
        pushMessage("error", `Prime daemon disconnected: ${payload.reason}`);
        return;
      }
      if (payload.activeSessionId !== sessionId) return;
      if (payload.kind === "sessionClosed") {
        sessionIdRef.current = null;
        streamingIdRef.current = null;
        setStatus("idle");
        setConfig(null);
        pushMessage("error", `Prime session closed: ${payload.reason}`);
        return;
      }
      const event = payload.event;
      const message = event.message as
        | { role?: string; stopReason?: string; errorMessage?: string }
        | undefined;
      switch (event.type) {
        case "agent_start":
          setStatus("streaming");
          break;
        case "agent_end":
          streamingIdRef.current = null;
          setStatus("idle");
          // A turn can fail over to another model or clamp the level.
          void api
            .primeSessionConfig(payload.activeSessionId)
            .then(setConfig)
            .catch(console.error);
          break;
        case "message_start":
          if (message?.role === "assistant") streamingIdRef.current = null;
          break;
        case "message_update": {
          const stream = event.assistantMessageEvent as
            | { type?: string; delta?: string }
            | undefined;
          if (message?.role === "assistant" && stream?.type === "text_delta" && stream.delta) {
            appendAssistantText(stream.delta);
          }
          break;
        }
        case "message_end":
          if (message?.role !== "assistant") break;
          streamingIdRef.current = null;
          if (message.stopReason === "error" && message.errorMessage) {
            pushMessage("error", message.errorMessage);
          }
          break;
      }
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [appendAssistantText, pushMessage]);

  // The session dies with the pane (conversation deleted or workspace
  // changed, see Workspace.tsx); one still being created closes on arrival.
  // The flag is reset on (re)mount: StrictMode mounts, unmounts and mounts
  // again in dev.
  useEffect(() => {
    unmountedRef.current = false;
    return () => {
      unmountedRef.current = true;
      const sessionId = sessionIdRef.current;
      sessionIdRef.current = null;
      if (sessionId) void api.primeCloseSession(sessionId).catch(console.error);
    };
  }, []);

  const ensureSession = useCallback((): Promise<string> => {
    if (sessionIdRef.current) return Promise.resolve(sessionIdRef.current);
    if (creatingRef.current) return creatingRef.current;
    setStatus("starting");
    const creating = (async () => {
      try {
        const sessionId = await api.primeCreateSession(workspacePath);
        if (unmountedRef.current) {
          void api.primeCloseSession(sessionId).catch(console.error);
          throw new Error("Prime pane closed");
        }
        sessionIdRef.current = sessionId;
        setConfig(await api.primeSessionConfig(sessionId).catch(() => null));
        return sessionId;
      } finally {
        creatingRef.current = null;
        setStatus((current) => (current === "starting" ? "idle" : current));
      }
    })();
    creatingRef.current = creating;
    return creating;
  }, [workspacePath]);

  useEffect(() => {
    if (!active || sessionIdRef.current || creatingRef.current) return;
    void ensureSession().catch((err) => {
      if (!unmountedRef.current) pushMessage("error", String(err));
    });
  }, [active, ensureSession, pushMessage]);

  useLayoutEffect(() => {
    const body = bodyRef.current;
    if (body) body.scrollTop = body.scrollHeight;
  }, [messages]);

  const send = useCallback(async () => {
    const prompt = text.trim();
    if (!prompt || status === "starting") return;
    setText("");
    pushMessage("user", prompt);
    try {
      const sessionId = await ensureSession();
      setStatus("streaming");
      await api.primePrompt(sessionId, prompt);
    } catch (err) {
      setStatus("idle");
      pushMessage("error", String(err));
    }
  }, [text, status, ensureSession, pushMessage]);

  const changeModel = useCallback(
    async (index: number) => {
      const sessionId = sessionIdRef.current;
      const model = config?.models[index];
      if (!sessionId || !model) return;
      setConfigBusy(true);
      try {
        setConfig(await api.primeSetModel(sessionId, model.provider, model.id));
      } catch (err) {
        pushMessage("error", String(err));
      } finally {
        setConfigBusy(false);
      }
    },
    [config, pushMessage],
  );

  const changeThinkingLevel = useCallback(
    async (level: string) => {
      const sessionId = sessionIdRef.current;
      if (!sessionId) return;
      setConfigBusy(true);
      try {
        setConfig(await api.primeSetThinkingLevel(sessionId, level));
      } catch (err) {
        pushMessage("error", String(err));
      } finally {
        setConfigBusy(false);
      }
    },
    [pushMessage],
  );

  const stop = useCallback(async () => {
    const sessionId = sessionIdRef.current;
    if (!sessionId) return;
    try {
      await api.primeAbort(sessionId);
    } catch (err) {
      pushMessage("error", String(err));
    }
  }, [pushMessage]);

  const busy = status !== "idle";
  const pickersDisabled = busy || configBusy || !config;
  const modelIndex = config?.model
    ? config.models.findIndex(
        (model) =>
          model.provider === config.model?.provider && model.id === config.model?.id,
      )
    : -1;

  return (
    <div className="chat-col prime-chat">
      <div className="chat-head">
        <span className="chat-head__title">
          <Icon
            icon="solar:chat-square-code-bold-duotone"
            width={16}
            height={16}
            style={{ color: "var(--text-3)" }}
          />
          <span>Prime</span>
        </span>
        {headerExtra}
        <span className="chat-head__dot" data-status={busy ? "streaming" : "idle"} />
      </div>
      <div className="chat-body" ref={bodyRef}>
        <div className="chat-body__content">
          {messages.length === 0 ? (
            <div className="chat-empty">
              <span className="chat-empty__mark">
                <Icon icon="solar:magic-stick-3-bold-duotone" width={22} height={22} />
              </span>
              <span className="chat-empty__title">Ask Prime Agent</span>
              <span className="chat-empty__sub">Enter to send · Shift+Enter for newline</span>
            </div>
          ) : (
            messages.map((message) =>
              message.role === "user" ? (
                <div key={message.id} className="msg" data-role="user">
                  <div className="msg__body user-text">{message.text}</div>
                </div>
              ) : message.role === "error" ? (
                <div key={message.id} className="msg" data-role="assistant">
                  <div className="msg__body prime-chat__error">{message.text}</div>
                </div>
              ) : (
                <div key={message.id} className="msg" data-role="assistant">
                  <div className="msg__body">
                    <Markdown text={message.text} onOpenFile={onOpenFile} />
                  </div>
                </div>
              ),
            )
          )}
          {status === "starting" && (
            <div className="prime-chat__status">Starting Prime…</div>
          )}
        </div>
      </div>
      <div className="composer">
        <div className="composer__box">
          <div className="composer__input-wrap">
            <textarea
              className="composer__input prime-chat__input"
              value={text}
              placeholder={busy ? "Queue next prompt..." : "Message Prime…"}
              onChange={(event) => setText(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) {
                  return;
                }
                event.preventDefault();
                void send();
              }}
            />
          </div>
          <div className="composer__actions">
            <div className="composer__actions-left prime-chat__pickers">
              {config && (
                <>
                  <select
                    className="prime-chat__select"
                    aria-label="Prime model"
                    title="Model"
                    value={modelIndex}
                    disabled={pickersDisabled}
                    onChange={(event) => void changeModel(Number(event.target.value))}
                  >
                    {modelIndex === -1 && config.model && (
                      <option value={-1}>{config.model.name}</option>
                    )}
                    {config.models.map((model, index) => (
                      <option key={`${model.provider}/${model.id}`} value={index}>
                        {model.name}
                      </option>
                    ))}
                  </select>
                  {config.availableThinkingLevels.some((level) => level !== "off") && (
                    <select
                      className="prime-chat__select"
                      aria-label="Prime thinking level"
                      title="Thinking level"
                      value={config.thinkingLevel ?? ""}
                      disabled={pickersDisabled}
                      onChange={(event) => void changeThinkingLevel(event.target.value)}
                    >
                      {config.availableThinkingLevels.map((level) => (
                        <option key={level} value={level}>
                          {thinkingLevelLabel(level)}
                        </option>
                      ))}
                    </select>
                  )}
                </>
              )}
            </div>
            <div className="composer__actions-right">
              {status === "streaming" && (
                <button
                  className="composer__send"
                  data-variant="stop"
                  onClick={() => void stop()}
                >
                  <span className="composer__send-label">Stop</span>
                </button>
              )}
              <button
                className="composer__send"
                onClick={() => void send()}
                disabled={!text.trim() || status === "starting"}
              >
                <span className="composer__send-label">Send</span>
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

function thinkingLevelLabel(level: string): string {
  return level === "xhigh" ? "XHigh" : level.charAt(0).toUpperCase() + level.slice(1);
}
