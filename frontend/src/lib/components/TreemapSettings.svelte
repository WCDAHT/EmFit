<!--
  TreemapSettings.svelte — edit the treemap coloring mode, persisted to the
  global config in appdata (core `service::config`, TOML).

  Two modes (WizTree parity): "ranked" assigns the 13-color WizTree palette
  to extensions by total size-on-disk rank (everything past the list gets
  the last, gray entry); "extension" colors by extension with a configurable
  extension→color list — the editable list is a FUTURE milestone, so today
  it falls back to the built-in category palette.
-->
<script lang="ts">
  import { getConfig, setConfig } from "../ipc";
  import { session, queryChanged } from "../session.svelte";

  interface Props {
    open: boolean;
    onClose: () => void;
  }
  let { open, onClose }: Props = $props();

  let mode = $state<"ranked" | "extension">("ranked");

  // Re-seed the editor from the live session every time it opens.
  $effect(() => {
    if (open) {
      mode = session.colorMode;
    }
  });

  async function save() {
    const config = await getConfig();
    // size_ranges passes through untouched: legacy data from the retired
    // size-bucket mode, kept so old configs round-trip.
    config.treemap = { color_mode: mode, size_ranges: session.sizeRanges };
    await setConfig(config);

    session.colorMode = mode;
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
    <div class="dialog" role="dialog" aria-modal="true" aria-label="Treemap colors">
      <header>
        <h2>Treemap colors</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      <label class="mode">
        Color files by
        <select bind:value={mode}>
          <option value="ranked">Extension, ranked by size (WizTree)</option>
          <option value="extension">Extension category</option>
        </select>
      </label>

      {#if mode === "ranked"}
        <p class="hint">
          The extensions using the most space on disk each get their own
          color from the WizTree palette, in order; every other extension is
          gray.
        </p>
      {:else}
        <p class="hint">
          Colors by built-in category (executables, archives, images, …).
          Assigning specific colors to specific extensions will be
          configurable here in a later milestone.
        </p>
      {/if}

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
    width: min(380px, 90vw);
    max-height: 80vh;
    overflow-y: auto;
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

  .mode {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
  .mode select {
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
