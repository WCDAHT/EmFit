<!--
  App.svelte — root view. No in-app header: chrome lives in the native window
  menu (built in src-tauri/src/lib.rs), so every pixel below the menu bar
  belongs to the data. Two tabs: List (search) and Tree view (the M3
  space-analysis pane, placeholder until then).

  All keyboard shortcuts dispatch from here (STANDARDS §3.7), and all Tauri
  events — scan progress, view updates, native menu clicks — are subscribed
  here once and projected into the shared session state.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import About from "./lib/components/About.svelte";
  import Settings from "./lib/components/Settings.svelte";
  import ShortcutsDialog from "./lib/components/ShortcutsDialog.svelte";
  import ScanControls from "./lib/views/ScanControls.svelte";
  import ScanStatus from "./lib/views/ScanStatus.svelte";
  import SearchBar from "./lib/views/SearchBar.svelte";
  import FileList from "./lib/views/FileList.svelte";
  import TreeView from "./lib/views/TreeView.svelte";
  import StatusBar from "./lib/views/StatusBar.svelte";
  import { toggleThemeMode, syncThemeWithConfig } from "./lib/theme";
  import { cancelScan, getConfig, listVolumes } from "./lib/ipc";
  import {
    session,
    hooks,
    queryChanged,
    clearSelection,
    selectAll,
  } from "./lib/session.svelte";
  import type {
    MenuZoomEvent,
    ScanDoneEvent,
    ScanProgressEvent,
    UsnDeletedEvent,
    UsnGapEvent,
    ViewUpdatedEvent,
  } from "./lib/types";
  import { SIZE_UNITS, formatSize, type SizeUnit } from "./lib/format";

  let aboutOpen = $state(false);
  let settingsOpen = $state(false);
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
              summary: `${payload.files.toLocaleString()} files, ${formatSize(payload.total_size)} in ${(payload.elapsed_ms / 1000).toFixed(1)}s`,
            }
          : {
              phase: "error",
              message: "",
              done: 0,
              total: null,
              error: payload.error ?? "failed",
            };

        // A scan REPLACES the volume set, so everything keyed by (vol, id)
        // is stale: deletion marks, the treemap drill, and the focus.
        if (payload.ok) {
          session.deletedNodes.clear();
          session.drill = null;
          session.focus = null;
        }

        const anyRunning = Object.values(session.scan).some((s) => s.phase === "scanning");
        if (!anyRunning) session.scanning = false;
        session.volumes = await listVolumes();
      }),

      // M5 deletion marks: the USN watcher saw these nodes deleted. Marks
      // are frontend-session state only, sticky until the next rescan.
      listen<UsnDeletedEvent>("usn:deleted", ({ payload }) => {
        console.log(
          `usn:deleted received — vol ${payload.vol}, ${payload.ids.length} node(s):`,
          payload.ids.slice(0, 10),
        );
        for (const id of payload.ids) {
          session.deletedNodes.add(`${payload.vol}:${id}`);
        }
        console.log(`deletion marks now: ${session.deletedNodes.size}`);
      }),

      // "Zoom in" from the shell context menu: drill the treemap into the
      // folder — switching to the Tree view if the menu came from the list.
      listen<MenuZoomEvent>("menu:zoom", ({ payload }) => {
        session.tab = "tree";
        session.drill = { vol: payload.vol, id: payload.id };
      }),

      listen<UsnGapEvent>("usn:gap", ({ payload }) => {
        session.warnings = [
          ...session.warnings,
          `Change journal gap on ${payload.volume} — deletion marks are incomplete; rescan to refresh.`,
        ];
      }),

      listen<ViewUpdatedEvent>("view:updated", ({ payload }) => {
        session.generation = payload.generation;
        session.total = payload.total;
        session.elapsedMs = payload.elapsed_ms;
        session.warnings = payload.warnings;
        session.volumesTotalBytes = payload.volumes_total_bytes;
        session.viewEpoch += 1;
        // The result set changed under the selection; positions are stale.
        clearSelection();
      }),

      // Native menu clicks arrive as ids (see lib.rs).
      listen<string>("menu", ({ payload }) => {
        switch (payload) {
          case "sources":
            session.sourcesOpen = true;
            break;
          case "rescan":
            if (!session.scanning) hooks.rescan?.();
            break;
          case "cancel_scan":
            void cancelScan();
            break;
          case "toggle_theme":
            toggleThemeMode();
            break;
          case "settings":
            settingsOpen = true;
            break;
          case "shortcuts":
            shortcutsOpen = true;
            break;
          case "about":
            aboutOpen = true;
            break;
        }
      }),
    ];

    // Reconcile the first-paint theme cache with the durable config, and
    // project the persisted treemap coloring into the session.
    void syncThemeWithConfig();
    void getConfig().then((cfg) => {
      session.sizeUnit = (SIZE_UNITS as readonly string[]).includes(cfg.size_unit)
        ? (cfg.size_unit as SizeUnit)
        : "dynamic";
      if (cfg.treemap) {
        session.colorMode = cfg.treemap.color_mode === "extension" ? "extension" : "ranked";
        session.showFreeSpace = cfg.treemap.show_free_space ?? false;
      }
    });

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
    const dialogOpen = aboutOpen || shortcutsOpen || settingsOpen;

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
      session.tab = "list";
      hooks.focusSearch?.();
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a" && !inTextInput(e)) {
      e.preventDefault();
      selectAll();
      return;
    }
    if (e.key === "Escape") {
      if (session.sourcesOpen) {
        session.sourcesOpen = false;
      } else if (session.scanning) {
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
  <div class="topbar">
    <nav class="tabs" aria-label="Views">
      <button
        class="tab"
        class:active={session.tab === "list"}
        onclick={() => (session.tab = "list")}
      >
        List
      </button>
      <button
        class="tab"
        class:active={session.tab === "tree"}
        onclick={() => (session.tab = "tree")}
      >
        Tree view
      </button>
    </nav>
    <div class="spacer"></div>
    <ScanControls />
  </div>

  <ScanStatus />

  {#if session.tab === "list"}
    <SearchBar />
    <FileList />
  {:else}
    <TreeView />
  {/if}

  <StatusBar />
</main>

<About open={aboutOpen} onClose={() => (aboutOpen = false)} />
<Settings open={settingsOpen} onClose={() => (settingsOpen = false)} />
<ShortcutsDialog open={shortcutsOpen} onClose={() => (shortcutsOpen = false)} />

<style>
  main {
    height: 100%;
    display: flex;
    flex-direction: column;
    padding: var(--space-2) var(--space-2) 0;
    gap: var(--space-2);
  }

  .topbar {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  .spacer {
    flex: 1;
  }

  .tabs {
    display: flex;
    gap: var(--space-1);
  }

  .tab {
    height: var(--button-height);
    padding: 0 var(--space-4);
    border: 1px solid transparent;
    border-radius: var(--radius-small);
    background: none;
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .tab:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .tab.active {
    border-color: var(--border-strong);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-weight: var(--font-weight-semibold);
  }
</style>
