import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { Icon } from "@iconify/react";
import { api } from "../../lib/ipc";
import { primeBashTitle, spawnCalls } from "../../lib/primeBash";
import {
  agentRow,
  historyToMessages,
  thinkingDuration,
  toolArgsPretty,
  toolCode,
  toolResultDetailsStatus,
  toolResultImages,
  toolResultText,
  toolSummary,
  type PrimeMessage,
  type PrimeTextMessage,
  type PrimeThinking,
  type PrimeToolCall,
} from "../../lib/primeHistory";
import { primeToolTitle } from "../../lib/primeToolTitle";
import { MODELS, PROVIDERS, THINKING_LEVELS } from "../../lib/models";
import type {
  PrimeEventPayload,
  PrimeImportReport,
  PrimeModelOption,
  PrimeProjectType,
  PrimeSessionConfig,
} from "../../types";
import { AIThinkingBlock } from "./AIThinkingBlock";
import { Markdown } from "./Markdown";
import { AiAgentGlyph, ToolCard, type ToolOutputLimit } from "./ToolCard";

// Minimal Prime Agent chat: one daemon session per pane (Workspace mounts
// one pane per yusAi conversation), opened the first time the pane is shown
// from the conversation's session file, so the thread, the model and the
// thinking level come back after a restart (prime_session.rs, open_thread).
// User prompts, assistant text and tool calls are rendered; thinking and
// sub-agents are out of scope for this milestone.

type PrimeStatus = "idle" | "starting" | "streaming";

// How often a pane on screen refreshes its sub-agents' status while one runs.
const SUB_AGENT_POLL_MS = 2000;

// `get_rlm_children` statuses (pa-daemon/src/rlm_children.rs:215-229).
function subAgentStatusLabel(status: string): string {
  if (status === "error") return "failed";
  return status;
}

// Tool output beyond this renders behind "Show all": a cell can print
// megabytes of stdout, and every streamed chunk re-renders the card.
const TOOL_OUTPUT_LIMIT: ToolOutputLimit = { chars: 20_000, lines: 200 };

type Props = {
  workspacePath: string;
  // The yusAi conversation whose Prime thread this pane shows.
  conversationId: string;
  // The pane is the one on screen (Prime engine, active conversation).
  active: boolean;
  headerExtra?: ReactNode;
  onOpenFile: (path: string) => void;
};

export function PrimeChatPane({
  workspacePath,
  conversationId,
  active,
  headerExtra,
  onOpenFile,
}: Props) {
  const [messages, setMessages] = useState<PrimeMessage[]>([]);
  const [status, setStatus] = useState<PrimeStatus>("idle");
  const [text, setText] = useState("");
  const [config, setConfig] = useState<PrimeSessionConfig | null>(null);
  const [configBusy, setConfigBusy] = useState(false);
  // Spawned sub-agents' status by name, from `get_rlm_children`.
  const [subAgentStatus, setSubAgentStatus] = useState<Record<string, string>>({});
  // Bumped when the daemon closes the session (idle passivation, another
  // window): a pane on screen reopens its thread from the file.
  const [closedCount, setClosedCount] = useState(0);
  const sessionIdRef = useRef<string | null>(null);
  // The in-flight session creation, shared by the eager start and a send.
  const creatingRef = useRef<Promise<string> | null>(null);
  const unmountedRef = useRef(false);
  const nextIdRef = useRef(1);
  // The assistant message currently receiving text deltas.
  const streamingIdRef = useRef<number | null>(null);
  // The thinking block currently receiving reasoning deltas.
  const thinkingIdRef = useRef<number | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);

  const pushMessage = useCallback((role: PrimeTextMessage["role"], value: string) => {
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
          message.id === target && message.role === "assistant"
            ? { ...message, text: message.text + delta }
            : message,
        ),
      );
    },
    [pushMessage],
  );

  // Reasoning streams as `thinking_delta` updates. The `thinking_start` frame
  // never reaches clients (the worker's coalescer replaces a delta-less frame
  // by the next delta, pa-daemon/src/streaming.rs:118-127), so the block
  // opens on its first delta and closes on `thinking_end`.
  const appendThinking = useCallback((delta: string) => {
    // Text after the reasoning starts a new bubble.
    streamingIdRef.current = null;
    let id = thinkingIdRef.current;
    if (id === null) {
      id = nextIdRef.current++;
      thinkingIdRef.current = id;
      const block: PrimeThinking = {
        id,
        role: "thinking",
        text: delta,
        streaming: true,
        startedAt: Date.now(),
      };
      setMessages((current) => [...current, block]);
      return;
    }
    const target = id;
    setMessages((current) =>
      current.map((message) =>
        message.id === target && message.role === "thinking"
          ? { ...message, text: message.text + delta }
          : message,
      ),
    );
  }, []);

  // Closes the streaming block: on `thinking_end`, or when the message, the
  // turn or the session ends without one (an abort).
  const settleThinking = useCallback(() => {
    const target = thinkingIdRef.current;
    if (target === null) return;
    thinkingIdRef.current = null;
    const now = Date.now();
    setMessages((current) =>
      current.map((message) =>
        message.id === target && message.role === "thinking"
          ? {
              ...message,
              streaming: false,
              durationMs: thinkingDuration(message.startedAt, now),
            }
          : message,
      ),
    );
  }, []);

  // Updates the newest card for the call (Prime's TUI does the same,
  // pa-tui/src/session_ui/apply.rs:1038-1041).
  const updateToolCall = useCallback(
    (toolCallId: string, update: (call: PrimeToolCall) => PrimeToolCall) => {
      setMessages((current) => {
        for (let index = current.length - 1; index >= 0; index--) {
          const message = current[index];
          if (message.role === "tool" && message.toolCallId === toolCallId) {
            const next = current.slice();
            next[index] = update(message);
            return next;
          }
        }
        return current;
      });
    },
    [],
  );

  // A call still running when its turn or session ends never gets its
  // `tool_execution_end`: settle it so no spinner is left behind.
  const settleRunningToolCalls = useCallback(() => {
    setMessages((current) =>
      current.some((message) => message.role === "tool" && message.status === "running")
        ? current.map((message) =>
            message.role === "tool" && message.status === "running"
              ? {
                  ...message,
                  status: "error",
                  isError: true,
                  note: undefined,
                  output: message.output || "Interrupted",
                }
              : message,
          )
        : current,
    );
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    void listen<PrimeEventPayload>("prime-event", ({ payload }) => {
      const sessionId = sessionIdRef.current;
      if (payload.kind === "disconnected") {
        if (!sessionId) return;
        sessionIdRef.current = null;
        streamingIdRef.current = null;
        settleThinking();
        setStatus("idle");
        setConfig(null);
        settleRunningToolCalls();
        pushMessage("error", `Prime daemon disconnected: ${payload.reason}`);
        return;
      }
      if (payload.activeSessionId !== sessionId) return;
      if (payload.kind === "sessionClosed") {
        sessionIdRef.current = null;
        streamingIdRef.current = null;
        settleThinking();
        setStatus("idle");
        setConfig(null);
        settleRunningToolCalls();
        setClosedCount((count) => count + 1);
        return;
      }
      if (payload.kind === "toolFileChanges") {
        updateToolCall(payload.toolCallId, (call) => ({
          ...call,
          fileChanges: payload.fileChanges,
        }));
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
          settleThinking();
          setStatus("idle");
          settleRunningToolCalls();
          // A turn can fail over to another model or clamp the level.
          void api
            .primeSessionConfig(payload.activeSessionId)
            .then(setConfig)
            .catch(console.error);
          break;
        case "message_start":
          if (message?.role === "assistant") streamingIdRef.current = null;
          // Custom rows arrive as a start + end pair; the start adds the row
          // (pa-tui/src/snapshot/decoder.rs:262-266).
          if (message?.role === "custom") {
            const row = agentRow(message as Record<string, unknown>, () => nextIdRef.current++);
            if (row) setMessages((current) => [...current, row]);
          }
          break;
        case "message_update": {
          const stream = event.assistantMessageEvent as
            | { type?: string; delta?: string }
            | undefined;
          if (message?.role !== "assistant") break;
          if (stream?.type === "thinking_delta" && stream.delta) {
            appendThinking(stream.delta);
          } else if (stream?.type === "thinking_end") {
            settleThinking();
          } else if (stream?.type === "text_delta" && stream.delta) {
            settleThinking();
            appendAssistantText(stream.delta);
          }
          break;
        }
        case "message_end":
          if (message?.role !== "assistant") break;
          streamingIdRef.current = null;
          settleThinking();
          if (message.stopReason === "error" && message.errorMessage) {
            pushMessage("error", message.errorMessage);
          }
          break;
        case "tool_execution_start": {
          const toolCallId = typeof event.toolCallId === "string" ? event.toolCallId : "";
          const name = typeof event.toolName === "string" ? event.toolName : "tool";
          const code = toolCode(event.args);
          streamingIdRef.current = null;
          const id = nextIdRef.current++;
          setMessages((current) => [
            ...current,
            {
              id,
              role: "tool",
              toolCallId,
              name,
              summary: toolSummary(name, event.args),
              argsPretty: toolArgsPretty(event.args),
              bash: code === undefined ? null : primeBashTitle(code, workspacePath),
              spawns: code === undefined ? [] : spawnCalls(code),
              status: "running",
              isError: false,
            },
          ]);
          break;
        }
        case "tool_execution_update": {
          if (typeof event.toolCallId !== "string") break;
          const partial = event.partialResult;
          const text = toolResultText(partial);
          if (text === undefined) break;
          const partialStatus = toolResultDetailsStatus(partial);
          updateToolCall(event.toolCallId, (call) =>
            partialStatus === "starting"
              ? // Kernel boot stage, one message per stage
                // (pa-core/src/tools/ipython.rs:392-396).
                { ...call, note: text }
              : partialStatus === "ok"
                ? // A new stdout/stderr chunk, not the output so far
                  // (ipython.rs:402-408, from kernel/manager/events.rs:161-179).
                  { ...call, note: undefined, output: (call.output ?? "") + text }
                : // Other tools: Prime's TUI treats a partial as the whole
                  // result so far (pa-tui/src/session_ui/apply.rs:1051).
                  { ...call, note: undefined, output: text },
          );
          break;
        }
        case "tool_execution_end": {
          if (typeof event.toolCallId !== "string") break;
          const isError = event.isError === true;
          const output = toolResultText(event.result);
          const images = toolResultImages(event.result);
          // The final result replaces whatever streamed in.
          updateToolCall(event.toolCallId, (call) => ({
            ...call,
            status: isError ? "error" : "done",
            isError,
            note: undefined,
            output: output ?? call.output,
            images: images.length > 0 ? images : undefined,
          }));
          break;
        }
      }
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [
    appendAssistantText,
    appendThinking,
    pushMessage,
    settleRunningToolCalls,
    settleThinking,
    updateToolCall,
    workspacePath,
  ]);

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
        const opened = await api.primeCreateSession(workspacePath, conversationId);
        const sessionId = opened.activeSessionId;
        if (unmountedRef.current) {
          void api.primeCloseSession(sessionId).catch(console.error);
          throw new Error("Prime pane closed");
        }
        sessionIdRef.current = sessionId;
        // The file is the source of truth: its thread replaces the pane's.
        streamingIdRef.current = null;
        thinkingIdRef.current = null;
        setMessages(
          historyToMessages(opened.messages, {
            nextId: () => nextIdRef.current++,
            bashTitle: (code) => primeBashTitle(code, workspacePath),
            spawnNames: spawnCalls,
          }),
        );
        // Read after the open: the worker has restored the file's model and
        // thinking level by then (pa-daemon/src/worker/create.rs:226-271).
        setConfig(await api.primeSessionConfig(sessionId).catch(() => null));
        return sessionId;
      } finally {
        creatingRef.current = null;
        setStatus((current) => (current === "starting" ? "idle" : current));
      }
    })();
    creatingRef.current = creating;
    return creating;
  }, [workspacePath, conversationId]);

  // A conversation shown in no window is eventually closed by the backend
  // (refine, then its worker is killed after 90 idle minutes, prime_close.rs).
  useEffect(() => {
    if (!active) return;
    void api.primeSetDisplayed(conversationId, true).catch(console.error);
    return () => {
      void api.primeSetDisplayed(conversationId, false).catch(console.error);
    };
  }, [active, conversationId]);

  useEffect(() => {
    if (!active || sessionIdRef.current || creatingRef.current) return;
    void ensureSession().catch((err) => {
      if (!unmountedRef.current) pushMessage("error", String(err));
    });
  }, [active, ensureSession, pushMessage, closedCount]);

  // Sub-agents run in their own sessions and push nothing to the parent
  // (pa-daemon/src/state_getters.rs:38-64): poll while one may be running.
  const hasSpawns = messages.some((message) => message.role === "tool" && message.spawns.length > 0);
  const subAgentRunning = Object.values(subAgentStatus).some((value) => value === "running");
  useEffect(() => {
    const sessionId = sessionIdRef.current;
    if (!active || !hasSpawns || !sessionId) return;
    let stopped = false;
    const poll = async () => {
      try {
        const children = await api.primeRlmChildren(sessionId);
        if (stopped) return;
        setSubAgentStatus(
          Object.fromEntries(children.map((child) => [child.sessionName, child.status])),
        );
      } catch (err) {
        console.error(err);
      }
    };
    void poll();
    if (status === "idle" && !subAgentRunning) {
      return () => {
        stopped = true;
      };
    }
    const timer = window.setInterval(() => void poll(), SUB_AGENT_POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [active, hasSpawns, subAgentRunning, status, config]);

  useLayoutEffect(() => {
    const body = bodyRef.current;
    if (body) body.scrollTop = body.scrollHeight;
  }, [messages]);

  const send = useCallback(async () => {
    const prompt = text.trim();
    if (!prompt || status === "starting") return;
    setText("");
    let shown = false;
    try {
      // Opened first: reopening the thread replaces the pane's messages.
      const sessionId = await ensureSession();
      pushMessage("user", prompt);
      shown = true;
      setStatus("streaming");
      await api.primePrompt(sessionId, prompt);
    } catch (err) {
      setStatus("idle");
      if (!shown) pushMessage("user", prompt);
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

  // "Remember" (« Retenir ») : a local refine of the conversation now,
  // imported as yusAi lessons (prime_session.rs, prime_retain).
  const [retaining, setRetaining] = useState(false);
  const [retainNote, setRetainNote] = useState<string | null>(null);
  const retain = useCallback(
    async (instructions: string) => {
      setRetaining(true);
      setRetainNote(null);
      try {
        const sessionId = await ensureSession();
        const report = await api.primeRetain(sessionId, instructions.trim() || null);
        setRetainNote(retainSummary(report));
      } catch (err) {
        setRetainNote("Remember failed");
        pushMessage("error", String(err));
      } finally {
        setRetaining(false);
      }
    },
    [ensureSession, pushMessage],
  );
  // The summary stays a few seconds in the header.
  useEffect(() => {
    if (!retainNote) return;
    const timer = window.setTimeout(() => setRetainNote(null), RETAIN_NOTE_MS);
    return () => window.clearTimeout(timer);
  }, [retainNote]);

  // The project's type, for type-level lessons: suggested from its files
  // until chosen here (prime_project.rs). Re-read whenever the pane shows.
  const [projectType, setProjectType] = useState<PrimeProjectType | null>(null);
  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    api
      .primeProjectType(workspacePath)
      .then((value) => {
        if (!cancelled) setProjectType(value);
      })
      .catch(console.error);
    return () => {
      cancelled = true;
    };
  }, [active, workspacePath]);
  const chooseProjectType = useCallback(
    async (value: string | null) => {
      try {
        setProjectType(await api.primeSetProjectType(workspacePath, value));
      } catch (err) {
        pushMessage("error", String(err));
      }
    },
    [workspacePath, pushMessage],
  );

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
        {projectType && (
          <ProjectTypePicker
            value={projectType}
            onChoose={(value) => void chooseProjectType(value)}
          />
        )}
        {headerExtra}
        <RetainButton
          running={retaining}
          disabled={busy || retaining || messages.length === 0}
          note={retainNote}
          onRetain={(instructions) => void retain(instructions)}
        />
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
              ) : message.role === "tool" ? (
                <div key={message.id} className="msg" data-role="assistant">
                  <ToolCard
                    name={message.name}
                    status={message.status}
                    summary={message.note ?? message.summary}
                    argsPretty={message.argsPretty}
                    output={message.output}
                    isError={message.isError}
                    images={message.images}
                    fileChanges={message.fileChanges}
                    outputLimit={TOOL_OUTPUT_LIMIT}
                    displayTitle={primeToolTitle(message, (name) => {
                      const value = subAgentStatus[name];
                      return value ? subAgentStatusLabel(value) : undefined;
                    })}
                    onOpenFile={onOpenFile}
                  />
                </div>
              ) : message.role === "agent" ? (
                <div key={message.id} className="msg" data-role="assistant">
                  <div className="prime-agent-row" data-kind={message.kind}>
                    <div className="prime-agent-row__head">
                      <AiAgentGlyph />
                      <span className="prime-agent-row__name">
                        {message.name ? `@${message.name}` : "Sub-agent"}
                      </span>
                      {message.kind === "notice" && (
                        <span className="prime-agent-row__notice">{message.text}</span>
                      )}
                    </div>
                    {message.kind === "message" && (
                      <div className="msg__body">
                        <Markdown text={message.text} onOpenFile={onOpenFile} />
                      </div>
                    )}
                  </div>
                </div>
              ) : message.role === "thinking" ? (
                <div key={message.id} className="msg" data-role="assistant">
                  <AIThinkingBlock
                    content={message.text}
                    isStreaming={message.streaming}
                    durationMs={message.durationMs}
                    onOpenFile={onOpenFile}
                  />
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

// The header's project type: a popover with the known types, "No type" and
// a free-text "Other type…", closed by an outside click or Escape. A
// suggested type is shown muted until confirmed (choosing it confirms it).
function ProjectTypePicker({
  value,
  onChoose,
}: {
  value: PrimeProjectType;
  onChoose: (projectType: string | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [other, setOther] = useState("");
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

  const choose = (projectType: string | null) => {
    setOpen(false);
    setOther("");
    onChoose(projectType);
  };
  const current = value.projectType;
  const suggested = value.source === "suggested";
  const sameType = (a: string, b: string) => a.trim().toLowerCase() === b.trim().toLowerCase();
  const types =
    current && !value.knownTypes.some((known) => sameType(known, current))
      ? [current, ...value.knownTypes]
      : value.knownTypes;

  return (
    <div className="prime-type" ref={ref}>
      <button
        type="button"
        className="prime-type__btn"
        data-suggested={suggested ? "true" : "false"}
        onClick={() => setOpen((now) => !now)}
        title={
          suggested
            ? "Project type suggested from its files, not used until you confirm it"
            : "Project type: lessons learned here can be shared with projects of the same type"
        }
      >
        <span>{current ?? "No type"}</span>
        <Icon icon="solar:alt-arrow-down-linear" width={11} height={11} />
      </button>
      {open && (
        <div className="prime-type__popover" role="menu" aria-label="Project type">
          {suggested && (
            <span className="prime-type__hint">
              {current
                ? "Suggested from the project's files, not used until confirmed"
                : "No type found in the project's files"}
            </span>
          )}
          {types.map((type) => {
            const selected = current !== null && sameType(type, current);
            return (
              <button
                key={type}
                type="button"
                className="composer__popover-row"
                data-selected={selected && !suggested ? "true" : "false"}
                onClick={() => choose(type)}
              >
                <span className="composer__popover-label">
                  <span>{type}</span>
                </span>
                {selected && (
                  <Icon
                    icon={suggested ? "solar:question-circle-linear" : "solar:check-read-linear"}
                    width={13}
                    height={13}
                    className="composer__popover-check"
                  />
                )}
              </button>
            );
          })}
          <button
            type="button"
            className="composer__popover-row"
            data-selected={current === null && !suggested ? "true" : "false"}
            onClick={() => choose(null)}
          >
            <span className="composer__popover-label">
              <span>No type</span>
            </span>
          </button>
          <input
            className="prime-type__input"
            placeholder="Other type…"
            value={other}
            onChange={(event) => setOther(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && other.trim()) {
                event.preventDefault();
                choose(other.trim());
              }
            }}
          />
        </div>
      )}
    </div>
  );
}

// How long the result of "Remember" stays in the header.
const RETAIN_NOTE_MS = 8000;

function plural(count: number, word: string): string {
  return `${count} ${word}${count === 1 ? "" : "s"}`;
}

// "Remembered: 2 lessons created · 1 updated · 1 proposal", or
// "Remembered: nothing new".
function retainSummary(report: PrimeImportReport): string {
  const parts = [];
  if (report.created.length) parts.push(`${plural(report.created.length, "lesson")} created`);
  if (report.updated.length) parts.push(`${report.updated.length} updated`);
  if (report.archived.length) parts.push(`${report.archived.length} archived`);
  if (report.proposals.length) parts.push(plural(report.proposals.length, "proposal"));
  if (report.failed.length) parts.push(`${report.failed.length} failed`);
  return `Remembered: ${parts.length ? parts.join(" · ") : "nothing new"}`;
}

// The header's "Remember" button: a popover with optional instructions
// ("focus on…"), closed by an outside click or Escape, like ComposerPicker.
function RetainButton({
  running,
  disabled,
  note,
  onRetain,
}: {
  running: boolean;
  disabled: boolean;
  note: string | null;
  onRetain: (instructions: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [instructions, setInstructions] = useState("");
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

  const submit = () => {
    setOpen(false);
    onRetain(instructions);
    setInstructions("");
  };

  return (
    <div className="prime-retain" ref={ref}>
      {note && !running && <span className="prime-retain__note">{note}</span>}
      <button
        type="button"
        className="prime-retain__btn"
        data-running={running ? "true" : "false"}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
        title={
          running
            ? "Remembering…"
            : "Turn what this conversation taught into lessons for the project"
        }
      >
        <Icon icon="solar:bookmark-linear" width={13} height={13} />
        <span>{running ? "Remembering…" : "Remember"}</span>
      </button>
      {open && !disabled && (
        <div className="prime-retain__popover" role="dialog" aria-label="Remember">
          <textarea
            className="prime-retain__input"
            rows={3}
            autoFocus
            placeholder="Focus on… (optional)"
            value={instructions}
            onChange={(event) => setInstructions(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                submit();
              }
            }}
          />
          <button type="button" className="prime-retain__submit" onClick={submit}>
            Remember
          </button>
        </div>
      )}
    </div>
  );
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
