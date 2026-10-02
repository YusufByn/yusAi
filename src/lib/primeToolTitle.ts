// The title of a Prime tool card, from what the cell did: a sub-agent spawn
// shows "Agent · <name>" with the child's status, a bash call shows its
// command (primeBash.ts), a cell that changed files shows the first changed
// path once its toolFileChanges arrive (prime_diffs.rs), anything else keeps
// the default title (the first line of the cell).
// Type-only imports keep it loadable by `node --test`.
import type { PrimeBashTitle } from "./primeBash";
import type { FileChange } from "../types";

export type PrimeToolTitle = {
  glyph: "terminal" | "edit" | "agent";
  main: string;
  // "+N" for the cell's other commands or changed files.
  meta?: string;
};

export function primeToolTitle(
  call: {
    bash: PrimeBashTitle | null;
    // The `name=` of each `rlm.spawn(...)` in the cell, null when not a literal.
    spawns?: (string | null)[];
    fileChanges?: FileChange[];
    // A kernel boot stage, shown instead while the call starts.
    note?: string;
  },
  // A spawned child's status by name (`get_rlm_children`).
  childStatus?: (name: string) => string | undefined,
): PrimeToolTitle | undefined {
  if (call.note) return undefined;
  if (call.spawns && call.spawns.length > 0) {
    const name = call.spawns[0];
    const status = name ? childStatus?.(name) : undefined;
    const meta = [status, moreLabel(call.spawns.length - 1)].filter(Boolean).join(" · ");
    return {
      glyph: "agent",
      main: name ? `Agent · ${name}` : "Agent",
      meta: meta || undefined,
    };
  }
  if (call.bash) {
    return { glyph: "terminal", main: call.bash.command, meta: moreLabel(call.bash.more) };
  }
  const first = call.fileChanges?.[0];
  if (first) {
    return {
      glyph: "edit",
      main: first.relativePath,
      meta: moreLabel(call.fileChanges!.length - 1),
    };
  }
  return undefined;
}

function moreLabel(more: number): string | undefined {
  return more > 0 ? `+${more}` : undefined;
}
