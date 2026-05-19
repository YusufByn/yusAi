/**
 * Theme preference (dark / light), persisted in localStorage and applied
 * via the `data-theme` attribute on `<html>`. CSS picks up the palette
 * through `[data-theme="dark"]` / `[data-theme="light"]` selectors in
 * `styles.css`.
 *
 * Dark stays the default — `:root` itself also carries the dark palette,
 * so the app degrades gracefully if for some reason the attribute isn't
 * applied yet (e.g. between the first paint and React mounting).
 */

export type Theme = "dark" | "light";

const STORAGE_KEY = "sinew.theme";
const DEFAULT_THEME: Theme = "dark";
const THEME_CHANGE_EVENT = "sinew:theme-change";

function isTheme(value: unknown): value is Theme {
  return value === "dark" || value === "light";
}

/** Read the persisted theme. Falls back to `"dark"` if missing or invalid. */
export function loadTheme(): Theme {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (isTheme(raw)) return raw;
  } catch {
    // localStorage can throw in private browsing / sandboxed contexts —
    // we silently fall back to the default.
  }
  return DEFAULT_THEME;
}

/**
 * Push a theme to the DOM. Cheap and idempotent — safe to call on every
 * render. Does NOT persist; use {@link setTheme} for that.
 */
export function applyTheme(theme: Theme): void {
  if (typeof document === "undefined") return;
  document.documentElement.dataset.theme = theme;
}

/**
 * Persist + apply a new theme. Dispatches a window event so any other
 * subscribed component (e.g. a second theme toggle elsewhere) can sync
 * without prop drilling.
 */
export function setTheme(theme: Theme): void {
  applyTheme(theme);
  try {
    window.localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // Persistence is best-effort: applying still works in-session.
  }
  try {
    window.dispatchEvent(
      new CustomEvent<Theme>(THEME_CHANGE_EVENT, { detail: theme }),
    );
  } catch {
    // CustomEvent should always exist in our WebView target, but guard
    // anyway so SSR / tests don't blow up.
  }
}

/** Convenience: flip dark<->light. */
export function toggleTheme(current: Theme): Theme {
  return current === "dark" ? "light" : "dark";
}

/** Subscribe to theme changes coming from anywhere in the app. */
export function subscribeTheme(listener: (theme: Theme) => void): () => void {
  const handler = (event: Event) => {
    const detail = (event as CustomEvent<Theme>).detail;
    if (isTheme(detail)) listener(detail);
  };
  window.addEventListener(THEME_CHANGE_EVENT, handler);
  return () => window.removeEventListener(THEME_CHANGE_EVENT, handler);
}
