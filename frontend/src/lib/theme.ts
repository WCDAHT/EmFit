//! Dark/light theme control. The CSS lives in styles/theme.css; this module
//! is the TypeScript counterpart to the old Slint `theme.rs`, owning the
//! `data-theme` attribute on <html> and persisting the user's choice.
//!
//! Per STANDARDS sec 2.2 the default is dark; light is the paper-white projector
//! palette. The choice should come from a user setting or system preference,
//! not be hardcoded at a call site.
//!
//! Source of truth is the backend config (`service::config`, STANDARDS sec 3.3):
//! the durable theme lives in `config.toml`. `localStorage` is kept only as a
//! synchronous first-paint cache - reading the config is async (IPC), too late
//! to set `data-theme` before the first frame, so we paint from the cache,
//! then reconcile with the backend on startup.

import { getConfig, setConfig } from "./ipc";
import { session } from "./session.svelte";

export type ThemeMode = "dark" | "light";

const STORAGE_KEY = "emfit.theme";

/** Read the cached choice, falling back to the OS preference, then dark. */
function preferredMode(): ThemeMode {
  const saved = localStorage.getItem(STORAGE_KEY);
  if (saved === "dark" || saved === "light") return saved;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

/** Apply `mode` to the document and update the first-paint cache. Does not
 *  touch the backend - use {@link saveThemeMode} to persist a user choice.
 *
 *  Bumps `session.themeEpoch` on a real change: DOM elements restyle
 *  through CSS alone, but canvas surfaces (the treemap scene) resolved
 *  their CSS variables at paint time and must be told to repaint. */
export function setThemeMode(mode: ThemeMode): void {
  const changed = document.documentElement.getAttribute("data-theme") !== mode;
  document.documentElement.setAttribute("data-theme", mode);
  localStorage.setItem(STORAGE_KEY, mode);
  if (changed) session.themeEpoch += 1;
}

/** Current mode as reflected on the document. */
export function getThemeMode(): ThemeMode {
  return document.documentElement.getAttribute("data-theme") === "light"
    ? "light"
    : "dark";
}

/** Flip between dark and light, returning the new mode. Applies + caches it
 *  and persists to the backend config (fire-and-forget). */
export function toggleThemeMode(): ThemeMode {
  const next: ThemeMode = getThemeMode() === "light" ? "dark" : "light";
  setThemeMode(next);
  void saveThemeMode(next);
  return next;
}

/** Persist the chosen theme to the backend config, preserving every other
 *  setting by round-tripping the whole config value. */
export async function saveThemeMode(mode: ThemeMode): Promise<void> {
  try {
    const config = await getConfig();
    config.theme = mode;
    await setConfig(config);
  } catch (e) {
    // Persistence is best-effort: the cache already reflects the choice, so a
    // transient backend hiccup never blocks the UI.
    console.error("Failed to persist theme to config:", e);
  }
}

/** Call once at startup (from main.ts) before the app mounts. Paints from the
 *  synchronous cache so there is no theme flash on the first frame. */
export function initTheme(): void {
  setThemeMode(preferredMode());
}

/** Reconcile the painted theme with the backend config after mount. If the
 *  config holds a theme it wins (and refreshes the cache); returns the mode now
 *  in effect so the caller can update its reactive state. */
export async function syncThemeWithConfig(): Promise<ThemeMode> {
  try {
    const config = await getConfig();
    if (config.theme === "dark" || config.theme === "light") {
      setThemeMode(config.theme);
      return config.theme;
    }
  } catch (e) {
    console.error("Failed to load theme from config:", e);
  }
  return getThemeMode();
}
