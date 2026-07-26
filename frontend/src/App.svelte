<!--
  App.svelte — root view: scan controls, search, the virtualized result list,
  and the status bar. First usable milestone (roadmap M2).

  All keyboard shortcuts dispatch from here (STANDARDS §3.7), and all Tauri
  events are subscribed here once and projected into the shared session
  state — child views read the session, they don't own copies.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import Icon from "./lib/components/Icon.svelte";
  import About from "./lib/components/About.svelte";
  import ShortcutsDialog from "./lib/components/ShortcutsDialog.svelte";
  import ScanBar from "./lib/views/ScanBar.svelte";
  import SearchBar from "./lib/views/SearchBar.svelte";
  import FileList from "./lib/views/FileList.svelte";
  import StatusBar from "./lib/views/StatusBar.svelte";
  import { toggleThemeMode, getThemeMode, syncThemeWithConfig, type ThemeMode } from "./lib/theme";
  import { cancelScan, listVolumes } from "./lib/ipc";
  import {
    session,
    hooks,
    queryChanged,
    clearSelection,
    selectAll,
  } from "./lib/session.svelte";
  import type { ScanDoneEvent, ScanProgressEvent, ViewUpdatedEvent } from "./lib/types";

  let theme = $state<ThemeMode>(getThemeMode());
  let aboutOpen = $state(false);
  let shortcutsOpen = $state(false);

  onMount(() => {
    const unlisteners: Promise<UnlistenFn>[] = [
      listen<ScanProgressEvent>("scan:progress", ({ payload }) => {
        const s = session.scan[payload.volume];
        if (!s) return;
        if (payload.kind === "Started") {
          s.message = payload.message;
          s.total = payload.total;
        } else if (payload.kind === "Tick") {
          s.done = payload.done;
        } else if (payload.kind === "Failed") {
          s.phase = "error";
          s.error = payload.message;
        }
      }),

      listen<ScanDoneEvent>("scan:done", async ({ payload }) => {
        session.scan[payload.volume] = payload.ok
          ? {
              phase: "done",
              message: "",
              done: 0,
              total: null,
              summary: `${payload.files.toLocaleString()} files, ${payload.total_display} in ${(payload.elapsed_ms / 1000).toFixed(1)}s`,
            }
          : {
              phase: "error",
              message: "",
              done: 0,
              total: null,
              error: payload.error ?? "failed",
            };

        const anyRunning = Object.values(session.scan).some((s) => s.phase === "scanning");
        if (!anyRunning) session.scanning = false;
        session.volumes = await listVolumes();
      }),

      listen<ViewUpdatedEvent>("view:updated", ({ payload }) => {
        session.generation = payload.generation;
        session.total = payload.total;
        session.elapsedMs = payload.elapsed_ms;
        session.warnings = payload.warnings;
        session.volumesTotalDisplay = payload.volumes_total_display;
        session.viewEpoch += 1;
        // The result set changed under the selection; positions are stale.
        clearSelection();
      }),
    ];

    // Reconcile the first-paint theme cache with the durable config.
    void syncThemeWithConfig().then((mode) => (theme = mode));

    return () => {
      for (const p of unlisteners) void p.then((unlisten) => unlisten());
    };
  });

  function inTextInput(e: KeyboardEvent): boolean {
    const t = e.target as HTMLElement | null;
    return !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
  }

  // All app shortcuts, dispatched at the root (STANDARDS §3.7). Open dialogs
  // own their own Esc; global handling is suppressed while one is up.
  function onKeydown(e: KeyboardEvent) {
    const dialogOpen = aboutOpen || shortcutsOpen;

    if (e.key === "F1") {
      e.preventDefault();
      if (!dialogOpen) shortcutsOpen = true;
      return;
    }
    if (dialogOpen) return;

    if (e.key === "F5") {
      e.preventDefault();
      if (!session.scanning) hooks.rescan?.();
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") {
      e.preventDefault();
      hooks.focusSearch?.();
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a" && !inTextInput(e)) {
      e.preventDefault();
      selectAll();
      return;
    }
    if (e.key === "Escape") {
      if (session.scanning) {
        void cancelScan();
      } else if (session.text !== "") {
        session.text = "";
        queryChanged(true);
      } else {
        clearSelection();
      }
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<main>
  <header class="titlebar">
    <h1>EmFit</h1>
    <div class="actions">
      <button
        class="icon-btn"
        title="Toggle light/dark"
        onclick={() => (theme = toggleThemeMode())}
      >
        <Icon name={theme === "light" ? "moon" : "sun"} />
      </button>
      <button
        class="icon-btn"
        title="Keyboard shortcuts (F1)"
        onclick={() => (shortcutsOpen = true)}
      >
        <Icon name="keyboard" />
      </button>
      <button class="icon-btn" title="About EmFit" onclick={() => (aboutOpen = true)}>
        <Icon name="info-circle" />
      </button>
    </div>
  </header>

  <ScanBar />
  <SearchBar />
  <FileList />
  <StatusBar />
</main>

<About open={aboutOpen} onClose={() => (aboutOpen = false)} />
<ShortcutsDialog open={shortcutsOpen} onClose={() => (shortcutsOpen = false)} />

<style>
  main {
    height: 100%;
    display: flex;
    flex-direction: column;
    padding: var(--space-3) var(--space-3) 0;
    gap: var(--space-2);
  }

  .titlebar {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  h1 {
    margin: 0;
    font-size: var(--font-size-heading);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .icon-btn {
    display: grid;
    place-items: center;
    width: var(--button-height);
    height: var(--button-height);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .icon-btn:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
</style>
