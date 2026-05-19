import type * as Monaco from "monaco-editor";
import type { Theme } from "./theme";

/**
 * Monaco editor theme definitions for Sinew. Monaco repaints its inner
 * widgets (gutter, scrollbar, suggest popup, …) via JS and cannot read
 * CSS variables — so we declare both palettes here and switch via
 * `monaco.editor.setTheme(monacoThemeName(currentTheme))`.
 *
 * Both `EditorPane` (file viewer) and `SettingsPane` (system prompt
 * editor) load the same themes so the editor looks consistent.
 */

const SINEW_DARK_THEME: Monaco.editor.IStandaloneThemeData = {
  base: "vs-dark",
  inherit: true,
  rules: [
    { token: "comment", foreground: "52555c" },
    { token: "keyword", foreground: "c4b5fd" },
    { token: "string", foreground: "86efac" },
    { token: "number", foreground: "f5a683" },
    { token: "type", foreground: "e8bb6a" },
    { token: "function", foreground: "9fc2ff" },
    { token: "variable", foreground: "e8e9ec" },
    { token: "constant", foreground: "f5a683" },
    { token: "regexp", foreground: "86efac" },
    { token: "tag", foreground: "f5a1ab" },
    { token: "attribute.name", foreground: "c4b5fd" },
  ],
  colors: {
    "editor.background": "#0b0b0d",
    "editor.foreground": "#e8e9ec",
    "editor.lineHighlightBackground": "#0f1013",
    "editorLineNumber.foreground": "#3a3d44",
    "editorLineNumber.activeForeground": "#9aa0a8",
    "editorCursor.foreground": "#3b82f6",
    "editor.selectionBackground": "#1e2b4a",
    "editor.inactiveSelectionBackground": "#141518",
    "editorIndentGuide.background1": "#141518",
    "editorIndentGuide.activeBackground1": "#23252b",
    "editorGutter.background": "#0b0b0d",
    "editorWidget.background": "#0f1013",
    "editorWidget.border": "#23252b",
    "editorHoverWidget.background": "#0f1013",
    "editorHoverWidget.border": "#23252b",
    "editorSuggestWidget.background": "#0f1013",
    "editorSuggestWidget.border": "#23252b",
    "editorSuggestWidget.selectedBackground": "#1e2b4a",
    "editorSuggestWidget.highlightForeground": "#5b8cff",
    "editorBracketMatch.background": "#1e2b4a",
    "editorBracketMatch.border": "#3b82f6",
    "scrollbarSlider.background": "#23252bcc",
    "scrollbarSlider.hoverBackground": "#2b2e35cc",
    "scrollbarSlider.activeBackground": "#3a3d44cc",
  },
};

const SINEW_LIGHT_THEME: Monaco.editor.IStandaloneThemeData = {
  base: "vs",
  inherit: true,
  rules: [
    { token: "comment", foreground: "6b7280" },
    { token: "keyword", foreground: "7c3aed" },
    { token: "string", foreground: "15803d" },
    { token: "number", foreground: "c2410c" },
    { token: "type", foreground: "a16207" },
    { token: "function", foreground: "1d4ed8" },
    { token: "variable", foreground: "1a1c1f" },
    { token: "constant", foreground: "c2410c" },
    { token: "regexp", foreground: "15803d" },
    { token: "tag", foreground: "be185d" },
    { token: "attribute.name", foreground: "7c3aed" },
  ],
  colors: {
    "editor.background": "#ffffff",
    "editor.foreground": "#1a1c1f",
    "editor.lineHighlightBackground": "#f7f8fa",
    "editorLineNumber.foreground": "#c5cad1",
    "editorLineNumber.activeForeground": "#5b6470",
    "editorCursor.foreground": "#2563eb",
    "editor.selectionBackground": "#dbeafe",
    "editor.inactiveSelectionBackground": "#eef0f3",
    "editorIndentGuide.background1": "#eef0f3",
    "editorIndentGuide.activeBackground1": "#dde1e7",
    "editorGutter.background": "#ffffff",
    "editorWidget.background": "#ffffff",
    "editorWidget.border": "#dde1e7",
    "editorHoverWidget.background": "#ffffff",
    "editorHoverWidget.border": "#dde1e7",
    "editorSuggestWidget.background": "#ffffff",
    "editorSuggestWidget.border": "#dde1e7",
    "editorSuggestWidget.selectedBackground": "#dbeafe",
    "editorSuggestWidget.highlightForeground": "#1d4ed8",
    "editorBracketMatch.background": "#dbeafe",
    "editorBracketMatch.border": "#2563eb",
    "scrollbarSlider.background": "#c5cad1cc",
    "scrollbarSlider.hoverBackground": "#a8aeb6cc",
    "scrollbarSlider.activeBackground": "#8a929ccc",
  },
};

export const SINEW_THEME_DARK_NAME = "sinew-cool";
export const SINEW_THEME_LIGHT_NAME = "sinew-cool-light";

/** Resolve a Theme value to the corresponding Monaco theme name. */
export function monacoThemeName(theme: Theme): string {
  return theme === "dark" ? SINEW_THEME_DARK_NAME : SINEW_THEME_LIGHT_NAME;
}

/** Register both themes on the given Monaco instance. Idempotent —
 *  defineTheme overrides any previous definition with the same name. */
export function defineSinewThemes(monaco: typeof Monaco): void {
  monaco.editor.defineTheme(SINEW_THEME_DARK_NAME, SINEW_DARK_THEME);
  monaco.editor.defineTheme(SINEW_THEME_LIGHT_NAME, SINEW_LIGHT_THEME);
}
