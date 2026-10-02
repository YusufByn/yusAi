// Run with `npm test` (Node runs the TypeScript directly).
import assert from "node:assert/strict";
import { test } from "node:test";
import { primeToolTitle } from "../src/lib/primeToolTitle.ts";
import type { FileChange } from "../src/types.ts";

function change(relativePath: string): FileChange {
  return {
    relativePath,
    kind: "modified",
    summary: "",
    binary: false,
    addedLines: 1,
    removedLines: 1,
    truncated: false,
    lines: [],
  };
}

test("a cell that changed one file shows its path", () => {
  assert.deepEqual(primeToolTitle({ bash: null, fileChanges: [change("src/a.rs")] }), {
    glyph: "edit",
    main: "src/a.rs",
    meta: undefined,
  });
});

test("several changed files show the first with a count", () => {
  assert.deepEqual(
    primeToolTitle({
      bash: null,
      fileChanges: [change("src/a.rs"), change("src/b.rs"), change("README.md")],
    }),
    { glyph: "edit", main: "src/a.rs", meta: "+2" },
  );
});

test("before the changed files arrive, the default title stays", () => {
  assert.equal(primeToolTitle({ bash: null }), undefined);
  assert.equal(primeToolTitle({ bash: null, fileChanges: [] }), undefined);
});

test("a bash cell keeps its command even when it changed files", () => {
  assert.deepEqual(
    primeToolTitle({
      bash: { command: "sed -i s/a/b/ a.txt", more: 0 },
      fileChanges: [change("a.txt")],
    }),
    { glyph: "terminal", main: "sed -i s/a/b/ a.txt", meta: undefined },
  );
  assert.deepEqual(primeToolTitle({ bash: { command: "ls", more: 1 } }), {
    glyph: "terminal",
    main: "ls",
    meta: "+1",
  });
});

test("the kernel boot note wins while the call starts", () => {
  assert.equal(
    primeToolTitle({
      bash: { command: "ls", more: 0 },
      fileChanges: [change("a.txt")],
      note: "Starting Python kernel...",
    }),
    undefined,
  );
});
