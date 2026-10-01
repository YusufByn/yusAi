import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { Icon } from "@iconify/react";
import { api } from "../../lib/ipc";
import type { PrimeEventPayload } from "../../types";
import { Markdown } from "./Markdown";

// Minimal Prime Agent chat: one daemon session per pane (Workspace mounts
// one pane per yusAi conversation), created on the first prompt. Only user prompts and assistant text are rendered; tool
// calls, thinking and sub-agents are out of scope for this milestone.

type PrimeMessage = {
  id: number;
  role: "user" | "assistant" | "error";
  text: string;
};

type PrimeStatus = "idle" | "starting" | "streaming";

type Props = {
  workspacePath: string;
  headerExtra?: ReactNode;
  onOpenFile: (path: string) => void;
};

export function PrimeChatPane({ workspacePath, headerExtra, onOpenFile }: Props) {
  const [messages, setMessages] = useState<PrimeMessage[]>([]);
  const [status, setStatus] = useState<PrimeStatus>("idle");
  const [text, setText] = useState("");
  const sessionIdRef = useRef<string | null>(null);
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
        pushMessage("error", `Prime daemon disconnected: ${payload.reason}`);
        return;
      }
      if (payload.activeSessionId !== sessionId) return;
      if (payload.kind === "sessionClosed") {
        sessionIdRef.current = null;
        streamingIdRef.current = null;
        setStatus("idle");
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
  // changed, see Workspace.tsx).
  useEffect(
    () => () => {
      const sessionId = sessionIdRef.current;
      sessionIdRef.current = null;
      if (sessionId) void api.primeCloseSession(sessionId).catch(console.error);
    },
    [],
  );

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
      let sessionId = sessionIdRef.current;
      if (!sessionId) {
        setStatus("starting");
        sessionId = await api.primeCreateSession(workspacePath);
        sessionIdRef.current = sessionId;
      }
      setStatus("streaming");
      await api.primePrompt(sessionId, prompt);
    } catch (err) {
      setStatus("idle");
      pushMessage("error", String(err));
    }
  }, [text, status, workspacePath, pushMessage]);

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
            <div className="composer__actions-left" />
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
