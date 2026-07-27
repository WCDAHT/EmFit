<!--
  TreemapSettings.svelte — edit the treemap coloring: mode (size buckets vs
  extension categories) and the size buckets themselves, both persisted to
  the global config in appdata (core `service::config`, TOML).
-->
<script lang="ts">
  import { getConfig, setConfig } from "../ipc";
  import { session, queryChanged } from "../session.svelte";
  import type { SizeRange } from "../types";

  interface Props {
    open: boolean;
    onClose: () => void;
  }
  let { open, onClose }: Props = $props();

  interface EditRange {
    maxText: string;
    color: string;
  }

  let mode = $state<"size" | "extension">("size");
  let ranges = $state<EditRange[]>([]);
  let error = $state("");

  // Re-seed the editor from the live session every time it opens.
  $effect(() => {
    if (open) {
      mode = session.colorMode;
      ranges = session.sizeRanges.map((r) => ({
        maxText: humanBytes(r.max_bytes),
        color: r.color,
      }));
      error = "";
    }
  });

  function humanBytes(bytes: number): string {
    const units = ["B", "KB", "MB", "GB", "TB"];
    let v = bytes;
    let u = 0;
    while (v >= 1024 && u < units.length - 1) {
      v /= 1024;
      u += 1;
    }
    const text = Number.isInteger(v) ? `${v}` : v.toFixed(1);
    return `${text} ${units[u]}`;
  }

  function parseBytes(text: string): number | null {
    const m = /^\s*([\d.]+)\s*(b|kb|kib|mb|mib|gb|gib|tb|tib)?\s*$/i.exec(text);
    if (!m) return null;
    const value = Number.parseFloat(m[1]);
    if (!Number.isFinite(value) || value < 0) return null;
    const unit = (m[2] ?? "b").toLowerCase();
    const power = { b: 0, kb: 1, kib: 1, mb: 2, mib: 2, gb: 3, gib: 3, tb: 4, tib: 4 }[unit] ?? 0;
    return Math.round(value * 1024 ** power);
  }

  function addRange() {
    ranges.push({ maxText: "1 GB", color: "#888888" });
  }

  function removeRange(i: number) {
    ranges.splice(i, 1);
  }

  async function save() {
    const parsed: SizeRange[] = [];
    for (const r of ranges) {
      const max = parseBytes(r.maxText);
      if (max === null) {
        error = `"${r.maxText}" is not a size — use forms like 500 KB, 16 MB, 1.5 GB`;
        return;
      }
      parsed.push({ max_bytes: max, color: r.color });
    }
    if (mode === "size" && parsed.length === 0) {
      error = "Size mode needs at least one bucket.";
      return;
    }
    parsed.sort((a, b) => a.max_bytes - b.max_bytes);

    const config = await getConfig();
    config.treemap = { color_mode: mode, size_ranges: parsed };
    await setConfig(config);

    session.colorMode = mode;
    session.sizeRanges = parsed;
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
          <option value="size">Size (WizTree-style buckets)</option>
          <option value="extension">Extension category</option>
        </select>
      </label>

      {#if mode === "size"}
        <div class="ranges">
          <div class="ranges-head">
            <span>Up to</span>
            <span>Color</span>
            <span></span>
          </div>
          {#each ranges as range, i (i)}
            <div class="range">
              <input class="max" bind:value={range.maxText} spellcheck="false" />
              <input class="color" type="color" bind:value={range.color} />
              <button
                class="remove"
                title="Remove bucket"
                disabled={ranges.length <= 1}
                onclick={() => removeRange(i)}
              >
                ×
              </button>
            </div>
          {/each}
          <button class="add" onclick={addRange}>+ Add bucket</button>
          <p class="hint">
            Buckets sort by size on save; anything larger than the last bucket
            takes its color.
          </p>
        </div>
      {/if}

      {#if error}
        <p class="error">{error}</p>
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

  .ranges {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .ranges-head,
  .range {
    display: grid;
    grid-template-columns: 1fr 56px 28px;
    align-items: center;
    gap: var(--space-2);
  }
  .ranges-head {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .range .max {
    height: 28px;
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  .range .color {
    width: 56px;
    height: 28px;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: none;
    cursor: pointer;
  }
  .range .remove {
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: none;
    color: var(--text-secondary);
    cursor: pointer;
  }
  .range .remove:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .add {
    align-self: flex-start;
    border: 1px dashed var(--border-strong);
    border-radius: var(--radius-small);
    background: none;
    padding: var(--space-1) var(--space-2);
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }

  .hint {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .error {
    margin: 0;
    color: var(--danger);
    font-size: var(--font-size-body);
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
