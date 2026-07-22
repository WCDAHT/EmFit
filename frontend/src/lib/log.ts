// Frontend → backend log bridge.
//
// The webview has no log file of its own, so every `console.*` call is mirrored
// into the Rust `tracing` pipeline (via the `log_event` command), giving one
// unified, greppable log file where a full chain of actions — frontend intents
// and backend mutations alike — can be traced. The original console output is
// kept so devtools still works during `tauri dev`.
//
// Install once at startup (main.ts). Idempotent. Best-effort: a failed forward
// is swallowed so logging can never break the UI or recurse.

import { logEvent } from "./ipc";

type Level = "trace" | "debug" | "info" | "warn" | "error";

// JS console method → tracing level. `log` and `info` both map to info.
const CONSOLE_LEVELS: Record<string, Level> = {
  log: "info",
  info: "info",
  debug: "debug",
  warn: "warn",
  error: "error",
};

/** Render one console argument into a stable string for the log line. Errors
 *  carry their stack; objects are JSON; everything else is stringified. */
function formatArg(a: unknown): string {
  if (typeof a === "string") return a;
  if (a instanceof Error) return a.stack ? `${a.name}: ${a.message}\n${a.stack}` : `${a.name}: ${a.message}`;
  try {
    return JSON.stringify(a);
  } catch {
    return String(a);
  }
}

/** Forward a log line to the Rust pipeline. Never throws. */
export function logToBackend(level: Level, message: string): void {
  // Fire-and-forget; swallow failures (e.g. before the IPC bridge is ready).
  void logEvent(level, message).catch(() => {});
}

let installed = false;
// Guards against a console call made *synchronously* while we forward a line
// (e.g. Tauri internals logging an error) re-entering and looping.
let forwarding = false;

/** Patch the global console so every frontend log is mirrored into the Rust log
 *  file, while still printing to devtools. Call once at startup. */
export function installConsoleBridge(): void {
  if (installed) return;
  installed = true;

  const target = console as unknown as Record<string, (...args: unknown[]) => void>;
  for (const [method, level] of Object.entries(CONSOLE_LEVELS)) {
    const original = target[method].bind(console);
    target[method] = (...args: unknown[]) => {
      original(...args);
      if (forwarding) return;
      forwarding = true;
      try {
        logToBackend(level, args.map(formatArg).join(" "));
      } catch {
        // Logging must never throw.
      } finally {
        forwarding = false;
      }
    };
  }
}
