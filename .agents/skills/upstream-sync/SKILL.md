---
name: upstream-sync
description: Merge the latest commits from the Paseru/sinew upstream into this yusAi fork. Auto-resolve branding conflicts, triage the rest, run health checks, never commit.
---

# Upstream sync skill

You are syncing this **yusAi** fork with its upstream open-source repo
`Paseru/sinew`. The fork strategy is **front-only rebrand**: the back keeps
the upstream identifier `sinew` everywhere (Cargo crates, Tauri productName,
IPC events, localStorage keys, MIME types). Only user-facing UI strings are
changed, via the single constant `APP_NAME` in `src/branding.ts`.

Your job is to merge upstream safely and hand back a clear report. **You
never commit and never push.** The user reviews and finishes the merge by
hand.

---

## Hard rules

1. **Never run `git commit`, `git push`, `git reset --hard`, `git checkout --theirs/--ours` on a whole file, or `git merge --abort` without explicit user approval.**
2. **Never auto-resolve a conflict outside of these exact files / patterns:**
   - The list of "branding-only" lines defined in the section below.
3. If you are unsure whether a conflict is branding-only, **leave it for the user**.
4. Stop and ask the user if:
   - The working tree is dirty before starting.
   - The remote `upstream` is missing or points elsewhere than `Paseru/sinew`.
   - A conflict touches Rust code, `Cargo.toml`, `tauri.conf.json`, IPC, or any back-end file.

---

## Step 1 — Preflight

Run these checks in order. Abort and ask the user if any fail:

```powershell
git status --porcelain          # must be empty
git remote -v                   # must list "upstream" -> Paseru/sinew
git rev-parse --abbrev-ref HEAD # note current branch for the report
```

If `upstream` is missing, **do not add it yourself**. Ask the user to run
`git remote add upstream https://github.com/Paseru/sinew.git`.

---

## Step 2 — Fetch and start the merge

```powershell
git fetch upstream
git log --oneline HEAD..upstream/main  # preview incoming commits
git merge upstream/main --no-commit --no-ff
```

`--no-commit --no-ff` is mandatory: it leaves the merge staged so you (and
the user) can inspect everything before any commit happens.

If `git merge` exits 0 with no conflict, jump to **Step 5** (rebrand sweep)
and **Step 6** (health checks).

---

## Step 3 — Triage conflicts

List conflicted files:

```powershell
git diff --name-only --diff-filter=U
```

Classify each file into one of three buckets and **report all three to the user**:

| Bucket | Examples | What you do |
|---|---|---|
| **branding-only** | A conflict whose only diff is "Sinew" vs "yusAi" / `{APP_NAME}` on otherwise-identical lines, in files under `src/`. | Auto-resolve (Step 4). |
| **front-logic** | TS/TSX/CSS/HTML conflicts that involve real code changes. | Leave the conflict markers in place. Summarize what upstream changed and what HEAD had. |
| **back / critical** | Anything under `crates/`, `src-tauri/`, `Cargo.toml`, `Cargo.lock`, `package.json`, `tauri.conf.json`, `*.rs`, `*.toml`, workflows, scripts. | Leave the conflict markers in place. Flag as **priority review** in the report. |

For each non-branding conflict, read the file and produce a 2–4 line
summary: "upstream changed X, our fork had Y, suggested resolution: …".
Suggestions only — do not apply them.

---

## Step 4 — Auto-resolve branding conflicts

A conflict block is **branding-only** if and only if **all** of these hold:

- The file path matches `src/**` (front-end).
- Inside every `<<<<<<<` … `=======` … `>>>>>>>` block in that file, the only
  textual difference between the two sides is one of:
  - `Sinew` ↔ `yusAi`
  - `Sinew` ↔ `{APP_NAME}`
  - The presence of an extra `import { APP_NAME } from "../branding";` (or
    similar relative path) on the HEAD side.
- The rest of the lines (whitespace, JSX structure, surrounding text) are
  identical on both sides.

If yes, resolve by **keeping the HEAD version** (our fork's branding) using
`apply_patch` to rewrite the conflicted region. Then `git add <file>` that
single file.

If any conflict block in the file fails the test above, **do not touch the
file at all** — even the branding-only blocks inside it. The user resolves
the whole file.

---

## Step 5 — Post-merge rebrand sweep

Even when upstream does not conflict, it may have introduced **new**
hardcoded `"Sinew"` strings in fresh files. Find them:

```powershell
# Search visible-text occurrences in the front-end only.
# Internal identifiers (events, MIME, localStorage keys, function names)
# are deliberately ignored — they must keep the upstream name.
Select-String -Path src\**\*.tsx,src\**\*.ts,src\**\*.css,src\**\*.html `
              -Pattern 'Sinew' -CaseSensitive
```

Then filter the results: a match is a **rebrand candidate** only if it is a
user-visible string (JSX text, attribute value rendered as text, HTML
title, CSS content). Skip:

- Comments (`//`, `/* */`, `<!-- -->`).
- Function / component / type / variable names (e.g. `SinewMark`,
  `defineSinewThemes`).
- localStorage keys (`"sinew.xxx"`).
- Custom DOM event names (`"sinew:xxx"`).
- MIME types (`"application/x-sinew-xxx"`).
- URLs pointing to `github.com/Paseru/sinew` (that is the upstream repo).

For each genuine candidate, propose a patch that:

1. Adds `import { APP_NAME } from "<relative>/branding";` if missing.
2. Replaces the literal `"Sinew"` with `{APP_NAME}` in JSX, or with
   `` `${APP_NAME}` `` in template strings, or with `APP_NAME` in plain JS.

Apply the patches yourself only if the candidate is **unambiguous**. If a
match could be either visible text or an internal identifier, list it in
the report and let the user decide.

---

## Step 6 — Health checks

After your auto-resolutions and rebrand sweep, run both:

```powershell
npx tsc --noEmit
cargo check --workspace
```

Capture their full output. Do not try to fix Rust errors — they almost
certainly come from an unresolved back-end conflict that the user must
handle.

---

## Step 7 — Final report

Return a markdown report with these sections, in this order:

1. **Summary line**: `N commits pulled from upstream/main, M conflicts (A branding-resolved, B front-logic pending, C back/critical pending), D new rebrand candidates auto-applied, E candidates need user decision.`
2. **Branding conflicts auto-resolved** — bullet list of files.
3. **Front-logic conflicts pending** — for each file: path + 2–4 line summary + your suggested resolution (advisory only).
4. **Back / critical conflicts pending** — same format, flagged as priority.
5. **Rebrand candidates auto-applied** — bullet list of `file:line` with the before / after.
6. **Rebrand candidates needing review** — bullet list of `file:line` with the surrounding context and why it is ambiguous.
7. **Health checks** — `tsc` result, `cargo check` result. Quote the first 20 lines of any error block, no more.
8. **Next steps for the user** — a numbered, copy-pasteable checklist (resolve files X/Y/Z, re-run `npx tsc --noEmit`, then `git commit` to finalize the merge).

End the report with this exact line so the user always sees it:

> **Merge is staged but not committed. Review the pending conflicts above, then commit by hand.**

