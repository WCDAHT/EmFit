<!--
  Settings.svelte — the app settings dialog, opened from the native View
  menu (no on-screen button). Persists to the global config in appdata
  (core `service::config`, TOML). This is the seed of M6's settings panel.

  Structure: a vertical tab rail on the left, one settings page on the
  right. Adding a future settings page is three steps:
    1. add an entry to `TABS`,
    2. add a `{:else if tab === "..."}` page block in the markup,
    3. seed/save its state in the `$effect(open)` / `save()` pair.
  Save applies every tab's state in one config write; Cancel discards all.

  Current pages: General (size units), Treemap (color mode — WizTree parity:
  "ranked" auto-assigns the 13-color palette by size rank; "extension" is
  the configurable per-extension mode whose editable list is a FUTURE
  milestone — plus the free-space toggle).
-->
<script lang="ts">
  import { getConfig, setConfig } from "../ipc";
  import { session, queryChanged } from "../session.svelte";
  import { SIZE_UNITS, type SizeUnit } from "../format";

  interface Props {
    open: boolean;
    onClose: () => void;
  }
  let { open, onClose }: Props = $props();

  const TABS = [
    { id: "general", label: "General" },
    { id: "treemap", label: "Treemap" },
  ] as const;
  type TabId = (typeof TABS)[number]["id"];
  let tab = $state<TabId>("general");

  // --- per-page editor state, seeded on open, written on Save ---
  let sizeUnit = $state<SizeUnit>("dynamic");
  let mode = $state<"ranked" | "extension">("ranked");
  let showFreeSpace = $state(false);

  // Re-seed from the live session every time the dialog opens.
  $effect(() => {
    if (open) {
      tab = "general";
      sizeUnit = session.sizeUnit;
      mode = session.colorMode;
      showFreeSpace = session.showFreeSpace;
    }
  });

  function unitLabel(u: SizeUnit): string {
    return u === "dynamic" ? "Dynamic (largest unit ≥ 1)" : u;
  }

  async function save() {
    const config = await getConfig();
    config.size_unit = sizeUnit;
    config.treemap = {
      color_mode: mode,
      show_free_space: showFreeSpace,
    };
    await setConfig(config);

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
                …). Assigning specific colors to specific extensions will be
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
