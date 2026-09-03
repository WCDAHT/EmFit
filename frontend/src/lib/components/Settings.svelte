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
    backgroundStatus,
    runBackgroundNow,
    syncBackgroundTask,
    openLogFolder,
  } from "../ipc";
  import { session, queryChanged } from "../session.svelte";
  import { SIZE_UNITS, type SizeUnit } from "../format";
  import type { BackgroundStatus, ScanInterval } from "../types";

  interface Props {
    open: boolean;
    onClose: () => void;
    /** The Updates page asks; App owns the dialog and decides. */
    onCheckUpdatesRequested?: () => void;
  }
  let { open, onClose, onCheckUpdatesRequested }: Props = $props();

  const TABS = [
    { id: "general", label: "General" },
    { id: "treemap", label: "Treemap" },
    { id: "background", label: "Background" },
    { id: "updates", label: "Updates" },
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
  let updateOnStart = $state(false);
  let updateLastChecked = $state("");
  let backgroundState = $state<BackgroundStatus | null>(null);
  let runningNow = $state(false);
  let ranNow = $state("");
  // What the cache holds, and what the last clear did. Read when the dialog
  // opens; clearing acts at once rather than waiting for Save, because it is
  // an action, not a setting.
  let cacheSize = $state("");
  let cacheCount = $state(0);
  let clearing = $state(false);
  let cleared = $state("");
  // Opening the log folder is an action, not a setting, so it happens on the
  // click rather than on Save. Only a failure is worth showing: on success the
  // file manager is already in front of the user saying so.
  let logsError = $state("");

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
        updateOnStart = cfg.update?.check_on_start ?? false;
        updateLastChecked = cfg.update?.last_checked ?? "";
      });
      backgroundError = "";
      ranNow = "";
      void refreshBackgroundStatus();
      cleared = "";
      logsError = "";
      void refreshCacheUsage();
    }
  });

  async function onOpenLogs() {
    logsError = "";
    try {
      await openLogFolder();
    } catch (e) {
      logsError = `Could not open the log folder: ${e}`;
    }
  }

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

  async function refreshBackgroundStatus() {
    backgroundState = await backgroundStatus();
  }

  /** RFC 3339 from Rust, rendered in the reader's own locale. */
  function when(iso: string): string {
    if (iso === "") return "never";
    const at = new Date(iso);
    return Number.isNaN(at.getTime()) ? iso : at.toLocaleString();
  }

  async function onRunNow() {
    runningNow = true;
    ranNow = "";
    try {
      await runBackgroundNow();
      // The task starts a separate process; it will not have finished by the
      // time this returns, so do not claim it has.
      ranNow = "Started. It runs in the background - reopen this page to see the result.";
    } catch (e) {
      ranNow = `Could not start it: ${e}`;
    } finally {
      runningNow = false;
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
    // Only the opt-in is edited here; the rest of the block is written by the
    // check itself and must survive the save.
    config.update = { ...config.update, check_on_start: updateOnStart };
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

            <div class="row">
              <button class="btn" onclick={() => void onOpenLogs()}>
                Open log folder
              </button>
            </div>
            <p class="hint">
              What EmFit recorded about scans, updates and errors. The place to
              look when something went wrong, and what to attach to a bug
              report.
            </p>
            {#if logsError}
              <p class="hint warn">{logsError}</p>
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

            {#if backgroundState}
              <div class="status">
                <p class="hint">
                  {backgroundState.registered
                    ? `Registered with Windows as "${backgroundState.task_name}".`
                    : "Not registered with Windows."}
                </p>
                {#if backgroundState.registered}
                  <p class="hint">
                    Last run: {when(backgroundState.last_run)}{backgroundState.last_result
                      ? ` - ${backgroundState.last_result}`
                      : ""}
                  </p>
                  {#if backgroundState.next_run}
                    <p class="hint">Next run: about {when(backgroundState.next_run)}</p>
                  {/if}
                  <div class="row">
                    <button class="btn" onclick={() => void onRunNow()} disabled={runningNow}>
                      {runningNow ? "Starting..." : "Run now"}
                    </button>
                    {#if ranNow}<span class="hint">{ranNow}</span>{/if}
                  </div>
                {/if}
              </div>
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
          {:else if tab === "updates"}
            <label class="toggle">
              <input type="checkbox" bind:checked={updateOnStart} />
              Check for a new version when EmFit opens
            </label>
            <p class="hint">
              Off unless you turn it on. With it on, EmFit asks the release
              site once at startup and says nothing unless there is a newer
              version than the one you are running. Only the version number
              is sent.
            </p>

            <div class="row">
              <button class="btn" onclick={() => onCheckUpdatesRequested?.()}>
                Check now
              </button>
              <span class="hint">
                {updateLastChecked
                  ? `Last checked ${when(updateLastChecked)}.`
                  : "Not checked yet."}
              </span>
            </div>

            <p class="hint">
              EmFit runs from a single file, so an update replaces that file
              and restarts. If the folder EmFit runs from cannot be written to
              - one under Program Files without Administrator, say - the
              download is kept and you are told why. Nothing happens without
              you asking.
            </p>
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

  .status {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
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
  /* The text beside a button is what gives; a button squeezed narrower than
     its label wraps inside a fixed height and spills out of its own box. */
  .row .btn {
    flex: none;
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
