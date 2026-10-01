import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { Icon } from "@iconify/react";
import { api } from "../../lib/ipc";
import { MODELS, PROVIDERS, THINKING_LEVELS } from "../../lib/models";
import type { PrimeEventPayload, PrimeModelOption, PrimeSessionConfig } from "../../types";
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
      <div className={`composer${busy ? " composer--selector-locked" : ""}`}>
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
            <div className="composer__actions-left">
              {config && (
                <>
                  <ComposerPicker
                    kind="model"
                    title={busy ? "Model locked while streaming" : "Model"}
                    disabled={pickersDisabled || config.models.length === 0}
                    selectedKey={config.model ? modelKey(config.model) : null}
                    label={config.model ? modelLabel(config.model) : "No models"}
                    options={config.models.map((model) => ({
                      key: modelKey(model),
                      label: modelLabel(model),
                      icon: PROVIDERS.find((provider) => provider.value === model.provider)
                        ?.icon,
                    }))}
                    onSelect={(key) => {
                      const index = config.models.findIndex(
                        (model) => modelKey(model) === key,
                      );
                      if (index >= 0) void changeModel(index);
                    }}
                  />
                  {config.availableThinkingLevels.some((level) => level !== "off") && (
                    <ComposerPicker
                      kind="thinking"
                      title={busy ? "Thinking locked while streaming" : "Thinking"}
                      disabled={pickersDisabled}
                      selectedKey={config.thinkingLevel}
                      label={thinkingLevelLabel(config.thinkingLevel ?? "off")}
                      options={config.availableThinkingLevels.map((level) => ({
                        key: level,
                        label: thinkingLevelLabel(level),
                      }))}
                      onSelect={(level) => void changeThinkingLevel(level)}
                    />
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

function modelKey(model: PrimeModelOption): string {
  return `${model.provider}:${model.id}`;
}

// The Sinew label for models both engines know ("Opus 5.5"), else Prime's.
function modelLabel(model: PrimeModelOption): string {
  return MODELS.find((entry) => entry.value === modelKey(model))?.label ?? model.name;
}

function thinkingLevelLabel(level: string): string {
  return THINKING_LEVELS.find((entry) => entry.value === level)?.label ?? level;
}

type PickerOption = { key: string; label: string; icon?: string };

// The Sinew composer picker (ChatPane.tsx, composer__picker*): a button
// opening a popover, closed by an outside click or Escape.
function ComposerPicker({
  kind,
  title,
  label,
  options,
  selectedKey,
  disabled,
  onSelect,
}: {
  kind: string;
  title: string;
  label: string;
  options: PickerOption[];
  selectedKey: string | null;
  disabled: boolean;
  onSelect: (key: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const onDoc = (event: MouseEvent) => {
      if (ref.current && !ref.current.contains(event.target as Node)) setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="composer__picker" data-kind={kind} ref={ref}>
      <button
        type="button"
        className="composer__picker-btn"
        data-open={open ? "true" : "false"}
        data-locked={disabled ? "true" : "false"}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
        title={title}
      >
        <span className="composer__picker-label">{label}</span>
        <Icon icon="solar:alt-arrow-down-linear" width={11} height={11} />
      </button>
      {open && !disabled && (
        <div className="composer__popover" role="menu" aria-label={title}>
          {options.map((option) => {
            const selected = option.key === selectedKey;
            return (
              <button
                key={option.key}
                type="button"
                className="composer__popover-row"
                data-selected={selected ? "true" : "false"}
                onClick={() => {
                  setOpen(false);
                  onSelect(option.key);
                }}
              >
                <span className="composer__popover-label">
                  {option.icon && <Icon icon={option.icon} width={13} height={13} />}
                  <span>{option.label}</span>
                </span>
                {selected && (
                  <Icon
                    icon="solar:check-read-linear"
                    width={13}
                    height={13}
                    className="composer__popover-check"
                  />
                )}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
