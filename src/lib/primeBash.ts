// Shell commands and sub-agent spawns in a Prime `ipython` cell. Prime's
// model only has the ipython tool and runs shell commands through the kernel helper
// `bash(command: str)` (vendor/prime-agent/prime-agent-runtime/src/rlm/bash.py:954),
// so `r = await bash("ls")` is a shell call and its card shows the command.
// Kept free of imports so `node --test` can load it as is.

export type PrimeBashTitle = {
  // The first command, without a leading `cd <project> &&`.
  command: string;
  // The other bash calls in the cell.
  more: number;
};

// The title for a cell calling bash, or null when it does not call it with
// a literal command.
export function primeBashTitle(code: string, projectPath?: string): PrimeBashTitle | null {
  const calls = bashCalls(code);
  const first = calls.find((command): command is string => command !== null);
  if (first === undefined) return null;
  const command = stripProjectCd(first.trim(), projectPath);
  if (!command) return null;
  return { command, more: calls.length - 1 };
}

// Every `bash(...)` call in order: the command when the first argument is a
// string literal, else null. Comments and string contents are skipped.
export function bashCalls(code: string): (string | null)[] {
  const calls: (string | null)[] = [];
  let index = 0;
  while (index < code.length) {
    const char = code[index];
    if (char === "#") {
      const end = code.indexOf("\n", index);
      index = end < 0 ? code.length : end;
      continue;
    }
    if (!isIdentifierChar(code[index - 1] ?? "")) {
      const literal = readStringLiteral(code, index);
      if (literal) {
        index = literal.end;
        continue;
      }
    }
    if (isIdentifierStart(char) && !isIdentifierChar(code[index - 1] ?? "")) {
      let end = index + 1;
      while (end < code.length && isIdentifierChar(code[end])) end++;
      const word = code.slice(index, end);
      if (word === "bash" && previousNonSpace(code, index) !== ".") {
        let cursor = skipSpace(code, end);
        if (code[cursor] === "(") {
          cursor = skipSpace(code, cursor + 1);
          const keyword = /^command\s*=(?!=)\s*/.exec(code.slice(cursor));
          if (keyword) cursor += keyword[0].length;
          const literal = readStringLiteral(code, cursor);
          calls.push(literal && literal.value !== null ? literal.value : null);
        }
      }
      index = end;
      continue;
    }
    index++;
  }
  return calls;
}

// Every `rlm.spawn(...)` call in order: the sub-agent's `name=` when it is a
// string literal, else null. The model is told to write
// `handle = await rlm.spawn('task', name='worker')`
// (vendor/prime-agent/crates/pa-core/src/prompts/layers/core.md:33), but a
// name can also come from a variable or an f-string with fields.
export function spawnCalls(code: string): (string | null)[] {
  const calls: (string | null)[] = [];
  let index = 0;
  while (index < code.length) {
    const char = code[index];
    if (char === "#") {
      const end = code.indexOf("\n", index);
      index = end < 0 ? code.length : end;
      continue;
    }
    if (!isIdentifierChar(code[index - 1] ?? "")) {
      const literal = readStringLiteral(code, index);
      if (literal) {
        index = literal.end;
        continue;
      }
    }
    if (isIdentifierStart(char) && !isIdentifierChar(code[index - 1] ?? "")) {
      let end = index + 1;
      while (end < code.length && isIdentifierChar(code[end])) end++;
      if (code.slice(index, end) === "spawn" && identifierBeforeDot(code, index) === "rlm") {
        const cursor = skipSpace(code, end);
        if (code[cursor] === "(") calls.push(spawnName(code, cursor + 1));
      }
      index = end;
      continue;
    }
    index++;
  }
  return calls;
}

// The identifier before `.<name>` at `index`, if any (`rlm` in `rlm.spawn`).
function identifierBeforeDot(code: string, index: number): string | null {
  let cursor = index - 1;
  while (cursor >= 0 && /\s/.test(code[cursor])) cursor--;
  if (code[cursor] !== ".") return null;
  cursor--;
  while (cursor >= 0 && /\s/.test(code[cursor])) cursor--;
  const end = cursor + 1;
  while (cursor >= 0 && isIdentifierChar(code[cursor])) cursor--;
  return end > cursor + 1 ? code.slice(cursor + 1, end) : null;
}

// The literal `name=` argument of the call whose arguments start at
// `start`, read at the call's top level; null when absent or not a plain
// literal (f-string fields included).
function spawnName(code: string, start: number): string | null {
  let depth = 0;
  let index = start;
  while (index < code.length) {
    const char = code[index];
    if (char === "#") {
      const end = code.indexOf("\n", index);
      index = end < 0 ? code.length : end;
      continue;
    }
    if (!isIdentifierChar(code[index - 1] ?? "")) {
      const literal = readStringLiteral(code, index);
      if (literal) {
        index = literal.end;
        continue;
      }
    }
    if (char === "(" || char === "[" || char === "{") depth++;
    else if (char === ")" || char === "]" || char === "}") {
      if (depth === 0) return null;
      depth--;
    } else if (
      depth === 0 &&
      code.startsWith("name", index) &&
      !isIdentifierChar(code[index - 1] ?? "") &&
      !isIdentifierChar(code[index + 4] ?? "")
    ) {
      const keyword = /^name\s*=(?!=)\s*/.exec(code.slice(index));
      if (keyword) {
        const valueStart = index + keyword[0].length;
        const prefix = /^[rRuUfFbB]{0,2}/.exec(code.slice(valueStart))?.[0] ?? "";
        const literal = readStringLiteral(code, valueStart);
        if (!literal || literal.value === null) return null;
        if (/[fF]/.test(prefix) && literal.value.includes("{")) return null;
        return literal.value;
      }
    }
    index++;
  }
  return null;
}

// Drops a leading `cd <project> &&` (Prime prefixes commands with the
// workspace it already runs in); a `cd` elsewhere is kept.
export function stripProjectCd(command: string, projectPath?: string): string {
  if (!projectPath) return command;
  const match = /^cd\s+(?:"([^"]*)"|'([^']*)'|(\S+))\s*&&\s*/.exec(command);
  if (!match) return command;
  const target = match[1] ?? match[2] ?? match[3] ?? "";
  if (trimTrailingSlashes(target) !== trimTrailingSlashes(projectPath)) return command;
  return command.slice(match[0].length).trim() || command;
}

function trimTrailingSlashes(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed || path;
}

// A Python string literal starting at `start` (with an optional r/u/f/b
// prefix): its decoded value (null when unterminated) and where it ends.
// f-string fields are kept as written.
function readStringLiteral(
  code: string,
  start: number,
): { value: string | null; end: number } | null {
  const match = /^([rRuUfFbB]{0,2})("""|'''|"|')/.exec(code.slice(start, start + 5));
  if (!match) return null;
  const raw = /[rR]/.test(match[1]);
  const quote = match[2];
  const triple = quote.length === 3;
  let index = start + match[0].length;
  let value = "";
  while (index < code.length) {
    if (code.startsWith(quote, index)) {
      return { value, end: index + quote.length };
    }
    const char = code[index];
    if (char === "\n" && !triple) break;
    if (char === "\\" && index + 1 < code.length) {
      const next = code[index + 1];
      value += raw ? char + next : unescape(next);
      index += 2;
      continue;
    }
    value += char;
    index++;
  }
  return { value: null, end: index };
}

function unescape(char: string): string {
  switch (char) {
    case "n":
      return "\n";
    case "t":
      return "\t";
    case "\n":
      return "";
    case "\\":
    case "'":
    case '"':
      return char;
    default:
      return `\\${char}`;
  }
}

function skipSpace(code: string, index: number): number {
  while (index < code.length && /\s/.test(code[index])) index++;
  return index;
}

function previousNonSpace(code: string, index: number): string {
  let cursor = index - 1;
  while (cursor >= 0 && /\s/.test(code[cursor])) cursor--;
  return cursor >= 0 ? code[cursor] : "";
}

function isIdentifierStart(char: string): boolean {
  return /[A-Za-z_]/.test(char);
}

function isIdentifierChar(char: string): boolean {
  return /[A-Za-z0-9_]/.test(char);
}
