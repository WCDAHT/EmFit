<!--
  Settings.svelte - the app settings dialog, opened from the native View
  menu (no on-screen button). Persists to the global config in appdata
  (core `service::config`, TOML). This is the seed of M6's settings panel.

  Structure: a vertical tab rail on the left, one settings page on the
  right. Adding a future settings page is three steps:
    1. add an entry to `TABS`,
    2. add a `{:else if tab === "..."}` page block in the markup,
    3. seed/save its state in the `$effect(open)` / `save()` pair.
  Save applies every tab's state in one config write; Cancel discards all.

  Current pages: General (size units), Treemap (color mode - WizTree parity:
  "ranked" auto-assigns the 13-color palette by size rank; "extension" is
  the configurable per-extension mode whose editable list is a FUTURE
  milestone - plus the free-space toggle).
-->
<script lang="ts">
  import {
    cacheUsage,
    clearCache,
    getConfig,
    setConfig,
    syncBackgroundTask,
  } from "../ipc";
  import { session, queryChanged } from "../session.svelte";
  import { SIZE_UNITS, type SizeUnit } from "../format";
  import type { ScanInterval } from "../types";

  interface Props {
    open: boolean;
    onClose: () => void;
  }
  let { open, onClose }: Props = $props();

  const TABS = [
    { id: "general", label: "General" },
    { id: "treemap", label: "Treemap" },
    { id: "background", label: "Background" },
  ] as const;

  /** The intervals offered, and how each reads. Deliberately a short list -
   *  an arbitrary number invites a five-minute setting that thrashes the
   *  disk for no one's benefit. */
  const INTERVALS: { id: ScanInterval; label: string }[] = [
    { id: "hourly", label: "Every hour" },
    { id: "sixhourly", label: "Every 6 hours" },
    { id: "daily", label: "Every day" },
    { id: "weekly", label: "Every week" },
  ];
  type TabId = (typeof TABS)[number]["id"];
  let tab = $state<TabId>("general");

  // --- per-page editor state, seeded on open, written on Save ---
  let sizeUnit = $state<SizeUnit>("dynamic");
  let mode = $state<"ranked" | "extension">("ranked");
  let showFreeSpace = $state(false);
  let cacheEnabled = $state(true);
  let backgroundOn = $state(false);
  let backgroundVolumes = $state<string[]>([]);
  let backgroundInterval = $state<ScanInterval>("sixhourly");
  /** What was loaded, so Save can tell whether anything here actually
   *  changed - registering the task prompts for Administrator, and prompting
   *  on every Save would teach people to click through it. */
  let backgroundWas = $state("");
  let backgroundError = $state("");
  // What the cache holds, and what the last clear did. Read when the dialog
  // opens; clearing acts at once rather than waiting for Save, because it is
  // an action, not a setting.
  let cacheSize = $state("");
  let cacheCount = $state(0);
  let clearing = $state(false);
  let cleared = $state("");

  // Re-seed from the live session every time the dialog opens.
  $effect(() => {
    if (open) {
      tab = "general";
      sizeUnit = session.sizeUnit;
      mode = session.colorMode;
      showFreeSpace = session.showFreeSpace;
      // Not mirrored in the session - nothing but this dialog reads them.
      void getConfig().then((cfg) => {
        cacheEnabled = cfg.cache_enabled;
        backgroundOn = cfg.background.enabled;
        backgroundVolumes = [...cfg.background.volumes];
        backgroundInterval = cfg.background.interval;
        backgroundWas = JSON.stringify(cfg.background);
      });
      backgroundError = "";
      cleared = "";
      void refreshCacheUsage();
    }
  });

  async function refreshCacheUsage() {
    const usage = await cacheUsage();
    cacheSize = usage.display;
    cacheCount = usage.count;
  }

  async function onClearCache() {
    clearing = true;
    const freed = cacheSize;
    const count = cacheCount;
    try {
      const usage = await clearCache();
      cacheSize = usage.display;
      cacheCount = usage.count;
      cleared =
        count === 0
          ? "There was nothing cached."
          : `Removed ${count} cached scan${count === 1 ? "" : "s"}, freeing ${freed}.`;
    } catch (e) {
      cleared = `Could not clear the cache: ${e}`;
    } finally {
      clearing = false;
    }
  }

  /** Scannable drives, from the same list the Sources popup offers. */
  const scannable = $derived(session.volumes.filter((v) => v.raw_scannable));

  function toggleVolume(key: string, on: boolean) {
    backgroundVolumes = on
      ? [...backgroundVolumes, key]
      : backgroundVolumes.filter((k) => k !== key);
  }

  function unitLabel(u: SizeUnit): string {
    return u === "dynamic" ? "Dynamic (largest unit >= 1)" : u;
  }

  async function save() {
    const config = await getConfig();
    config.size_unit = sizeUnit;
    config.cache_enabled = cacheEnabled;
    config.treemap = {
      color_mode: mode,
      show_free_space: showFreeSpace,
    };
    config.background = {
      enabled: backgroundOn,
      volumes: backgroundVolumes,
      interval: backgroundInterval,
    };
    await setConfig(config);

    // Only when something here moved: this is the one setting whose Save asks
    // Windows for permission.
    if (JSON.stringify(config.background) !== backgroundWas) {
      try {
        backgroundOn = await syncBackgroundTask();
      } catch (e) {
        // A dismissed prompt, or a scheduler that refused. Say so and leave
        // the switch showing what is actually true rather than what was asked
        // for - the config is saved either way, so reopening Settings and
        // saving again retries it.
        backgroundError = `Windows did not accept the change: ${e}`;
        backgroundOn = false;
        config.background.enabled = false;
        await setConfig(config);
        tab = "background";
        return;
      }
    }

    session.sizeUnit = sizeUnit;
    session.colorMode = mode;
    session.showFreeSpace = showFreeSpace;
    queryChanged(true); // repaints the view epoch downstream
    onClose();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") onClose();
  }
</script>

<svelte:window onkeydown={open ? onKeydown : undefined} />

{#if open}
  <div
    class="backdrop"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) onClose();
    }}
  >
    <div class="dialog" role="dialog" aria-modal="true" aria-label="Settings">
      <header>
        <h2>Settings</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      <div class="body">
        <nav class="rail" aria-label="Settings sections">
          {#each TABS as t (t.id)}
            <button
              class="rail-tab"
              class:active={tab === t.id}
              onclick={() => (tab = t.id)}
            >
              {t.label}
            </button>
          {/each}
        </nav>

        <div class="page">
          {#if tab === "general"}
            <label class="field">
              Size units
              <select bind:value={sizeUnit}>
                {#each SIZE_UNITS as u (u)}
                  <option value={u}>{unitLabel(u)}</option>
                {/each}
              </select>
            </label>
            <p class="hint">
              Applies to every size shown anywhere in the app. Byte units
              step by 1024, bit units by 1000.
            </p>

            <label class="toggle">
              <input type="checkbox" bind:checked={cacheEnabled} />
              Reuse the last scan of a drive
            </label>
            <p class="hint">
              Scanning a drive reloads its last scan and asks the change
              journal what has changed since, re-reading only those files.
              The result is the same as a full scan, in a fraction of the
              time. Turn this off to always read the whole file table.
            </p>

            <div class="row">
              <button
                class="btn"
                onclick={() => void onClearCache()}
                disabled={clearing || cacheCount === 0}
              >
                {clearing ? "Clearing..." : "Clear cached scans"}
              </button>
              <span class="hint">
                {cacheCount === 0
                  ? "Nothing cached yet."
                  : `${cacheCount} cached scan${cacheCount === 1 ? "" : "s"}, ${cacheSize}.`}
              </span>
            </div>
            {#if cleared}
              <p class="hint">{cleared}</p>
            {/if}
          {:else if tab === "background"}
            <label class="toggle">
              <input type="checkbox" bind:checked={backgroundOn} />
              Keep selected drives scanned in the background
            </label>
            <p class="hint">
              Windows runs a scan on its own schedule, so opening EmFit shows
              a current index instead of starting one. Off unless you turn it
              on, and turning it off again removes it.
            </p>

            <fieldset class="drives" disabled={!backgroundOn}>
              <legend>Drives to keep scanned</legend>
              {#if scannable.length === 0}
                <p class="hint">No scannable drives were found.</p>
              {:else}
                {#each scannable as v (v.name)}
                  <label class="toggle">
                    <input
                      type="checkbox"
                      checked={backgroundVolumes.includes(v.name)}
                      onchange={(e) => toggleVolume(v.name, e.currentTarget.checked)}
                    />
                    {v.name}{v.label ? ` (${v.label})` : ""}
                  </label>
                {/each}
              {/if}
            </fieldset>

            <label class="field">
              How often
              <select bind:value={backgroundInterval} disabled={!backgroundOn}>
                {#each INTERVALS as i (i.id)}
                  <option value={i.id}>{i.label}</option>
                {/each}
              </select>
            </label>

            {#if backgroundOn && backgroundVolumes.length === 0}
              <p class="hint warn">
                No drives are selected, so nothing would be scanned.
              </p>
            {/if}
            {#if backgroundError}
              <p class="hint warn">{backgroundError}</p>
            {/if}
            <p class="hint">
              Saving a change here asks Windows for permission, because it
              adds or removes a scheduled task. It is called "EmFit Background
              Scan" and can also be removed from Task Scheduler.
            </p>
          {:else if tab === "treemap"}
            <label class="field">
              Color files by
              <select bind:value={mode}>
                <option value="ranked">Extension, ranked by size (WizTree)</option>
                <option value="extension">Extension category</option>
              </select>
            </label>

            {#if mode === "ranked"}
              <p class="hint">
                The extensions using the most space on disk each get their
                own color from the WizTree palette, in order; every other
                extension is gray.
              </p>
            {:else}
              <p class="hint">
                Colors by built-in category (executables, archives, images,
                ...). Assigning specific colors to specific extensions will be
                configurable here in a later milestone.
              </p>
            {/if}

            <label class="toggle">
              <input type="checkbox" bind:checked={showFreeSpace} />
              Show free space as a block
            </label>
          {/if}
        </div>
      </div>

      <footer>
        <button class="btn" onclick={onClose}>Cancel</button>
        <button class="btn primary" onclick={() => void save()}>Save</button>
      </footer>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    display: grid;
    place-items: center;
    z-index: 100;
  }

  .dialog {
    width: min(580px, 92vw);
    height: min(420px, 80vh);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-5);
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: var(--radius-medium);
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  h2 {
    margin: 0;
    font-size: var(--font-size-heading);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }
  .close {
    border: none;
    background: none;
    color: var(--text-secondary);
    font-size: var(--font-size-title);
    line-height: 1;
    cursor: pointer;
  }

  .body {
    flex: 1;
    min-height: 0;
    display: flex;
    gap: var(--space-4);
  }

  .rail {
    flex: 0 0 128px;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    border-right: 1px solid var(--border);
    padding-right: var(--space-3);
  }
  .rail-tab {
    height: var(--button-height);
    padding: 0 var(--space-3);
    border: none;
    border-radius: var(--radius-small);
    background: none;
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-body);
    text-align: left;
    cursor: pointer;
  }
  .rail-tab:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .rail-tab.active {
    background: var(--selection-soft);
    color: var(--text-primary);
    font-weight: var(--font-weight-semibold);
  }

  .page {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .field {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
  .field select {
    flex: 1;
    height: var(--button-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }

  .hint {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .hint.warn {
    color: var(--warning);
  }

  /* The drive list dims wholesale with the master switch, so an unticked
     switch cannot look like it is still going to scan something. */
  .drives {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
  }
  .drives:disabled {
    opacity: 0.5;
  }
  .drives legend {
    padding: 0 var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
  }

  .row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  footer {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
  .btn {
    height: var(--button-height);
    padding: 0 var(--space-4);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--text-on-accent);
    font-weight: var(--font-weight-semibold);
  }
</style>
