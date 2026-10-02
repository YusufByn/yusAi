// Run with `npm test` (Node runs the TypeScript directly).
import assert from "node:assert/strict";
import { test } from "node:test";
import { bashCalls, primeBashTitle, spawnCalls } from "../src/lib/primeBash.ts";

const PROJECT = "/Users/me/Desktop/yusAi";

test("single-quoted call", () => {
  assert.deepEqual(primeBashTitle("bash('ls -la')", PROJECT), { command: "ls -la", more: 0 });
});

test("awaited triple-quoted call", () => {
  const code = 'await bash("""\ngit status --short\n""")';
  assert.deepEqual(primeBashTitle(code, PROJECT), {
    command: "git status --short",
    more: 0,
  });
});

test("assigned awaited call", () => {
  const code = 'r = await bash("cargo test -p sinew-app")\nprint(r.output)';
  assert.deepEqual(primeBashTitle(code, PROJECT), {
    command: "cargo test -p sinew-app",
    more: 0,
  });
});

test("call nested in print", () => {
  assert.deepEqual(primeBashTitle("print(bash('ls'))", PROJECT), { command: "ls", more: 0 });
});

test("drops a leading cd into the project", () => {
  const code = `r = await bash("cd ${PROJECT} && git log --oneline -5")`;
  assert.deepEqual(primeBashTitle(code, PROJECT), {
    command: "git log --oneline -5",
    more: 0,
  });
});

test("drops a quoted cd into the project, with a trailing slash", () => {
  const code = `await bash("cd '${PROJECT}/' && npm run build")`;
  assert.deepEqual(primeBashTitle(code, PROJECT), { command: "npm run build", more: 0 });
});

test("keeps a cd elsewhere", () => {
  const code = 'await bash("cd /tmp && ls")';
  assert.deepEqual(primeBashTitle(code, PROJECT), { command: "cd /tmp && ls", more: 0 });
});

test("keeps a cd into a project subdirectory", () => {
  const code = `await bash("cd ${PROJECT}/src-tauri && cargo check")`;
  assert.deepEqual(primeBashTitle(code, PROJECT), {
    command: `cd ${PROJECT}/src-tauri && cargo check`,
    more: 0,
  });
});

test("several calls show the first with a count", () => {
  const code = [
    'a = await bash("git status --short")',
    "b = await bash('git diff --stat')",
    'c = bash("""npm test""")',
  ].join("\n");
  assert.deepEqual(primeBashTitle(code, PROJECT), {
    command: "git status --short",
    more: 2,
  });
});

test("not bash", () => {
  assert.equal(primeBashTitle("import os\nprint(os.listdir('.'))", PROJECT), null);
  assert.equal(primeBashTitle("edit('src/a.rs', [{'oldText': 'a', 'newText': 'b'}])", PROJECT), null);
});

test("ignores bash in comments, strings and other names", () => {
  const code = [
    "# bash('rm -rf /')",
    "print(\"bash('nope')\")",
    "run_bash('x')",
    "shell.bash('y')",
  ].join("\n");
  assert.deepEqual(bashCalls(code), []);
  assert.equal(primeBashTitle(code, PROJECT), null);
});

test("command keyword, raw and f-strings", () => {
  assert.deepEqual(bashCalls("bash(command='ls')"), ["ls"]);
  assert.deepEqual(bashCalls("bash(r'grep -n \\d+ a.txt')"), ["grep -n \\d+ a.txt"]);
  assert.deepEqual(bashCalls('bash(f"ls {path}")'), ["ls {path}"]);
});

test("decodes escapes in plain strings", () => {
  assert.deepEqual(bashCalls("bash('echo \\'hi\\'')"), ["echo 'hi'"]);
  assert.deepEqual(bashCalls('bash("printf \\"a\\\\n\\"")'), ['printf "a\\n"']);
});

test("a non-literal command is counted but not shown", () => {
  const code = 'cmd = "ls"\nawait bash(cmd)\nawait bash("pwd")';
  assert.deepEqual(bashCalls(code), [null, "pwd"]);
  assert.deepEqual(primeBashTitle(code, PROJECT), { command: "pwd", more: 1 });
  assert.equal(primeBashTitle("await bash(cmd)", PROJECT), null);
});

test("spawn names: literal, variable, f-string, several calls", () => {
  assert.deepEqual(spawnCalls("handle = await rlm.spawn('sub-task', name='worker')"), ["worker"]);
  assert.deepEqual(
    spawnCalls('await rlm.spawn(\n    "relis " + path,\n    model="anthropic/x",\n    name="reviewer",\n)'),
    ["reviewer"],
  );
  assert.deepEqual(spawnCalls("await rlm.spawn(task, name=agent_name)"), [null]);
  assert.deepEqual(spawnCalls('await rlm.spawn(task, name=f"worker-{i}")'), [null]);
  assert.deepEqual(spawnCalls('await rlm.spawn(task, name=f"worker")'), ["worker"]);
  assert.deepEqual(
    spawnCalls("a = await rlm.spawn('x', name='a')\nb = await rlm.spawn('y', name='b')"),
    ["a", "b"],
  );
});

test("spawn: nested names, comments, strings and other callees are ignored", () => {
  assert.deepEqual(spawnCalls("await rlm.spawn(fmt(name='inner'), name='outer')"), ["outer"]);
  assert.deepEqual(spawnCalls("# await rlm.spawn('x', name='a')"), []);
  assert.deepEqual(spawnCalls("print(\"rlm.spawn('x', name='a')\")"), []);
  assert.deepEqual(spawnCalls("other.spawn('x', name='a')\nspawn('x', name='b')"), []);
  assert.deepEqual(spawnCalls("await rlm.spawn('x', name='a') == 1"), ["a"]);
});
