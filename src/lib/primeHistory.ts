// The Prime chat's thread model and the readers of Prime's wire messages:
// tool call arguments and results, and the saved history the attach
// snapshot returns (pa-daemon/src/worker/connection.rs:860-894), turned
// into the same items the live events build. Type-only imports and an
// injected bash parser keep it loadable by `node --test`.
import type { PrimeBashTitle } from "./primeBash";
import type { ToolCardProps } from "../components/chat/ToolCard";
import type { FileChange, ToolResultImage } from "../types";

export type PrimeMessage =
  | PrimeTextMessage
  | PrimeThinking
  | PrimeToolCall
  | PrimeAgentMessage;

// A sub-agent's row in its parent's thread: its reply (`agent_message`,
// pa-core/src/session_engine/agent_messaging.rs:303-326) or the notice of an
// abnormal end (`rlm_child_terminal_notice` / `rlm_child_failure`,
// pa-core/src/session_engine/rlm_notices.rs:41-118).
export type PrimeAgentMessage = {
  id: number;
  role: "agent";
  kind: "message" | "notice";
  // The sender's `sessionName` (the `name=` of its spawn), when known.
  name: string | null;
  text: string;
};

// The model's reasoning, summarized by the provider: live from the
// `thinking_delta` stream events, restored from `thinking` content blocks.
export type PrimeThinking = {
  id: number;
  role: "thinking";
  text: string;
  streaming: boolean;
  // Client-side clock while streaming; restored blocks have no duration.
  startedAt?: number;
  durationMs?: number;
};

export type PrimeTextMessage = {
  id: number;
  role: "user" | "assistant" | "error";
  text: string;
};

// One tool call, from `tool_execution_start` to `tool_execution_end`
// (pa-daemon/src/worker/turn.rs:905-932).
export type PrimeToolCall = {
  id: number;
  role: "tool";
  toolCallId: string;
  name: string;
  summary: string;
  argsPretty?: string;
  // The cell's shell command, when it calls bash.
  bash: PrimeBashTitle | null;
  // The `name=` of each `rlm.spawn(...)` in the cell (null when not a
  // literal); empty when it spawns nothing.
  spawns: (string | null)[];
  // Kernel boot stage shown as the title while the call runs.
  note?: string;
  output?: string;
  status: ToolCardProps["status"];
  isError: boolean;
  images?: ToolResultImage[];
  // Files the call changed in the workspace (prime_diffs.rs).
  fileChanges?: FileChange[];
};

// Prime's model only has the `ipython` tool, whose sole argument is `code`
// (pa-core/src/tools/ipython.rs:348-359); bash and edit run inside it.
export function toolCode(args: unknown): string | undefined {
  if (!args || typeof args !== "object") return undefined;
  const code = (args as Record<string, unknown>).code;
  return typeof code === "string" ? code : undefined;
}

// The card title: the first line of the cell, else the tool name.
export function toolSummary(name: string, args: unknown): string {
  const line = toolCode(args)
    ?.split("\n")
    .map((value) => value.trim())
    .find(Boolean);
  if (!line) return name;
  return line.length > 120 ? `${line.slice(0, 120)}…` : line;
}

// The cell as written, or the raw arguments as JSON for other tools.
export function toolArgsPretty(args: unknown): string | undefined {
  const code = toolCode(args);
  if (code !== undefined) return code;
  if (args === undefined || args === null) return undefined;
  try {
    return JSON.stringify(args, null, 2);
  } catch {
    return undefined;
  }
}

// The text blocks of a `{content, details}` tool result, joined like the
// ACP bridge does (pa-daemon/src/acp/events.rs:393-408).
export function toolResultText(result: unknown): string | undefined {
  if (typeof result === "string") return result;
  if (!result || typeof result !== "object") return undefined;
  const content = (result as Record<string, unknown>).content;
  if (!Array.isArray(content)) return undefined;
  return content
    .filter(
      (block): block is { type: "text"; text: string } =>
        !!block &&
        typeof block === "object" &&
        (block as Record<string, unknown>).type === "text" &&
        typeof (block as Record<string, unknown>).text === "string",
    )
    .map((block) => block.text)
    .join("\n");
}

export function toolResultDetailsStatus(result: unknown): string | undefined {
  if (!result || typeof result !== "object") return undefined;
  const details = (result as Record<string, unknown>).details;
  if (!details || typeof details !== "object") return undefined;
  const status = (details as Record<string, unknown>).status;
  return typeof status === "string" ? status : undefined;
}

// Image blocks are `{type: "image", data, mimeType}` (pa-agent/src/types.rs:95-101).
export function toolResultImages(result: unknown): ToolResultImage[] {
  if (!result || typeof result !== "object") return [];
  const content = (result as Record<string, unknown>).content;
  if (!Array.isArray(content)) return [];
  const images: ToolResultImage[] = [];
  for (const block of content) {
    if (!block || typeof block !== "object") continue;
    const record = block as Record<string, unknown>;
    if (
      record.type === "image" &&
      typeof record.data === "string" &&
      typeof record.mimeType === "string"
    ) {
      images.push({ media_type: record.mimeType, data: record.data });
    }
  }
  return images;
}

// The saved thread as chat items, in order: user prompts, thinking, assistant
// text, assistant errors, and one tool card per `toolCall` block completed by
// its `toolResult` message. A call without a result (the turn was cut short)
// shows as interrupted. Empty or redacted thinking, custom rows and other
// roles are skipped.
// Changed files are not in the session file: restored cards have none.
export function historyToMessages(
  history: unknown[],
  options: {
    nextId: () => number;
    bashTitle: (code: string) => PrimeBashTitle | null;
    spawnNames: (code: string) => (string | null)[];
  },
): PrimeMessage[] {
  const items: PrimeMessage[] = [];
  const toolIndex = new Map<string, number>();
  for (const raw of history) {
    if (!raw || typeof raw !== "object") continue;
    const message = raw as Record<string, unknown>;
    if (message.role === "user") {
      const text = messageText(message.content);
      if (text) items.push({ id: options.nextId(), role: "user", text });
    } else if (message.role === "assistant") {
      const blocks = Array.isArray(message.content)
        ? message.content
        : [{ type: "text", text: message.content }];
      let text = "";
      const flushText = () => {
        if (text.trim()) items.push({ id: options.nextId(), role: "assistant", text });
        text = "";
      };
      for (const block of blocks) {
        if (!block || typeof block !== "object") continue;
        const record = block as Record<string, unknown>;
        if (record.type === "text" && typeof record.text === "string") {
          text += record.text;
        } else if (record.type === "thinking") {
          // `{type, thinking, thinkingSignature?, redacted?}`
          // (pa-agent/src/types.rs:77-91).
          const thinking = typeof record.thinking === "string" ? record.thinking : "";
          if (record.redacted === true || !thinking.trim()) continue;
          flushText();
          items.push({ id: options.nextId(), role: "thinking", text: thinking, streaming: false });
        } else if (record.type === "toolCall" && typeof record.id === "string") {
          flushText();
          const name = typeof record.name === "string" ? record.name : "tool";
          const code = toolCode(record.arguments);
          toolIndex.set(record.id, items.length);
          items.push({
            id: options.nextId(),
            role: "tool",
            toolCallId: record.id,
            name,
            summary: toolSummary(name, record.arguments),
            argsPretty: toolArgsPretty(record.arguments),
            bash: code === undefined ? null : options.bashTitle(code),
            spawns: code === undefined ? [] : options.spawnNames(code),
            status: "error",
            isError: true,
            output: "Interrupted",
          });
        }
      }
      flushText();
      if (message.stopReason === "error" && typeof message.errorMessage === "string") {
        items.push({ id: options.nextId(), role: "error", text: message.errorMessage });
      }
    } else if (message.role === "custom") {
      const row = agentRow(message, options.nextId);
      if (row) items.push(row);
    } else if (message.role === "toolResult" && typeof message.toolCallId === "string") {
      const index = toolIndex.get(message.toolCallId);
      const call = index === undefined ? undefined : items[index];
      if (!call || call.role !== "tool") continue;
      const isError = message.isError === true;
      const images = toolResultImages(message);
      items[index!] = {
        ...call,
        status: isError ? "error" : "done",
        isError,
        output: toolResultText(message),
        images: images.length > 0 ? images : undefined,
      };
    }
  }
  return items;
}

// A sub-agent row from a `custom` message, live (`message_start`) or
// restored; null for any other custom row.
export function agentRow(
  message: Record<string, unknown>,
  nextId: () => number,
): PrimeAgentMessage | null {
  const details =
    message.details && typeof message.details === "object"
      ? (message.details as Record<string, unknown>)
      : {};
  const text = (value: unknown) => (typeof value === "string" ? value : "");
  if (message.customType === "agent_message") {
    const from =
      details.from && typeof details.from === "object"
        ? (details.from as Record<string, unknown>)
        : {};
    return {
      id: nextId(),
      role: "agent",
      kind: "message",
      name: text(from.sessionName) || null,
      text: text(details.message) || messageText(message.content),
    };
  }
  if (message.customType === "rlm_child_terminal_notice") {
    const reason =
      details.kind === "cancelled"
        ? `Cancelled${text(details.reason) ? `: ${text(details.reason)}` : ""}`
        : details.kind === "completed_without_reply"
          ? "Finished without replying"
          : messageText(message.content);
    return {
      id: nextId(),
      role: "agent",
      kind: "notice",
      name: text(details.sessionName) || null,
      text: reason,
    };
  }
  if (message.customType === "rlm_child_failure") {
    return {
      id: nextId(),
      role: "agent",
      kind: "notice",
      name: text(details.sessionName) || null,
      text: `Failed${text(details.error) ? `: ${text(details.error)}` : ""}`,
    };
  }
  return null;
}

function messageText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter(
      (block): block is { type: "text"; text: string } =>
        !!block &&
        typeof block === "object" &&
        (block as Record<string, unknown>).type === "text" &&
        typeof (block as Record<string, unknown>).text === "string",
    )
    .map((block) => block.text)
    .join("");
}
