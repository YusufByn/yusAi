// Run with `npm test` (Node runs the TypeScript directly).
import assert from "node:assert/strict";
import { test } from "node:test";
import { historyToMessages } from "../src/lib/primeHistory.ts";

function convert(history: unknown[]) {
  let id = 1;
  return historyToMessages(history, {
    nextId: () => id++,
    bashTitle: (code) => (code.includes("bash(") ? { command: "ls", more: 0 } : null),
    spawnNames: (code) => (code.includes("rlm.spawn(") ? ["kid"] : []),
  });
}

test("prompts, text and a completed tool call", () => {
  const items = convert([
    { role: "user", content: [{ type: "text", text: "liste les fichiers" }] },
    {
      role: "assistant",
      content: [
        { type: "thinking", thinking: "hmm" },
        { type: "text", text: "Je regarde." },
        { type: "toolCall", id: "c1", name: "ipython", arguments: { code: "print(bash('ls'))" } },
      ],
      stopReason: "toolUse",
    },
    {
      role: "toolResult",
      toolCallId: "c1",
      toolName: "ipython",
      content: [{ type: "text", text: "README.md" }],
      isError: false,
    },
    { role: "assistant", content: [{ type: "text", text: "Un fichier." }], stopReason: "stop" },
  ]);
  assert.deepEqual(items, [
    { id: 1, role: "user", text: "liste les fichiers" },
    { id: 2, role: "thinking", text: "hmm", streaming: false },
    { id: 3, role: "assistant", text: "Je regarde." },
    {
      id: 4,
      role: "tool",
      toolCallId: "c1",
      name: "ipython",
      summary: "print(bash('ls'))",
      argsPretty: "print(bash('ls'))",
      bash: { command: "ls", more: 0 },
      spawns: [],
      status: "done",
      isError: false,
      output: "README.md",
      images: undefined,
    },
    { id: 5, role: "assistant", text: "Un fichier." },
  ]);
});

test("thinking keeps its place; empty and redacted blocks are skipped", () => {
  const items = convert([
    {
      role: "assistant",
      content: [
        { type: "text", text: "Avant." },
        { type: "thinking", thinking: "  " },
        { type: "thinking", thinking: "", thinkingSignature: "sig", redacted: true },
        { type: "thinking", thinking: "Je pèse." },
        { type: "text", text: "Après." },
      ],
    },
  ]);
  assert.deepEqual(items, [
    { id: 1, role: "assistant", text: "Avant." },
    { id: 2, role: "thinking", text: "Je pèse.", streaming: false },
    { id: 3, role: "assistant", text: "Après." },
  ]);
});

test("a failed tool call and an assistant error", () => {
  const items = convert([
    {
      role: "assistant",
      content: [{ type: "toolCall", id: "c1", name: "ipython", arguments: { code: "edit(x)" } }],
    },
    {
      role: "toolResult",
      toolCallId: "c1",
      content: [{ type: "text", text: "ValueError" }],
      isError: true,
    },
    { role: "assistant", content: [], stopReason: "error", errorMessage: "overloaded" },
  ]);
  assert.equal(items.length, 2);
  assert.equal(items[0].role, "tool");
  assert.equal(items[0].role === "tool" && items[0].status, "error");
  assert.equal(items[0].role === "tool" && items[0].output, "ValueError");
  assert.deepEqual(items[1], { id: 2, role: "error", text: "overloaded" });
});

test("a call without a result shows as interrupted", () => {
  const items = convert([
    {
      role: "assistant",
      content: [{ type: "toolCall", id: "c1", name: "ipython", arguments: { code: "x = 1" } }],
    },
  ]);
  assert.equal(items.length, 1);
  assert.equal(items[0].role === "tool" && items[0].status, "error");
  assert.equal(items[0].role === "tool" && items[0].output, "Interrupted");
});

test("string content, custom rows and unknown roles", () => {
  const items = convert([
    { role: "user", content: "salut" },
    { role: "custom", customType: "refinement_notice", content: "..." },
    { role: "assistant", content: "bonjour" },
    { role: "toolResult", toolCallId: "nope", content: [] },
  ]);
  assert.deepEqual(items, [
    { id: 1, role: "user", text: "salut" },
    { id: 2, role: "assistant", text: "bonjour" },
  ]);
});

test("sub-agent spawns, replies and abnormal ends", () => {
  const items = convert([
    {
      role: "assistant",
      content: [
        { type: "toolCall", id: "c1", name: "ipython", arguments: { code: "await rlm.spawn('t', name='kid')" } },
      ],
    },
    { role: "toolResult", toolCallId: "c1", content: [{ type: "text", text: "ok" }], isError: false },
    {
      role: "custom",
      customType: "agent_message",
      display: true,
      content: "Agent-to-agent message received. ...",
      details: {
        message: "fini du kid",
        from: { sessionName: "kid", runtimeKind: "subagent" },
        fromRelationship: "child",
      },
    },
    {
      role: "custom",
      customType: "rlm_child_terminal_notice",
      content: "[child-exited: no-reply child:kid]",
      details: { kind: "completed_without_reply", sessionName: "kid" },
    },
    {
      role: "custom",
      customType: "rlm_child_failure",
      content: "[child-failed child:bob]",
      details: { sessionName: "bob", error: "boom" },
    },
    { role: "custom", customType: "refinement_notice", content: "..." },
  ]);
  assert.equal(items[0].role === "tool" && items[0].spawns.join(), "kid");
  assert.deepEqual(items.slice(1), [
    { id: 2, role: "agent", kind: "message", name: "kid", text: "fini du kid" },
    { id: 3, role: "agent", kind: "notice", name: "kid", text: "Finished without replying" },
    { id: 4, role: "agent", kind: "notice", name: "bob", text: "Failed: boom" },
  ]);
});
