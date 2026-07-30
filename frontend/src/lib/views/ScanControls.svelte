<!--
  ScanControls.svelte — the Sources popup and the Scan/Cancel button.

  The scan list is managed, not toggled: the popup shows what will be
  scanned (each entry removable with ×) and what could be added (each
  volume with +, plus "Add disk image…" for your own). Native volumes all
  appear by default; only C: starts on the list.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
  import { cancelScan, elevationStatus, listVolumes, startScan } from "../ipc";
  import { formatSize } from "../format";
  import { session, hooks, addTarget, removeTarget } from "../session.svelte";
  import Icon from "../components/Icon.svelte";

  let starting = $state(false);
  let popup: HTMLDivElement | undefined = $state();

  onMount(async () => {
    hooks.rescan = () => void scan();
    session.elevated = await elevationStatus();
    session.volumes = await listVolumes();

    // Native disks are all offered; only C: is enabled out of the box.
    if (session.targets.length === 0) {
      const c = session.volumes.find((v) => v.name === "C:" && v.raw_scannable);
      const first = c ?? session.volumes.find((v) => v.raw_scannable);
      if (first) {
        addTarget({ kind: "volume", key: first.name, label: first.name });
      }
    }
  });

  async function scan() {
    if (session.targets.length === 0 || session.scanning || starting) return;
    starting = true;
    try {
      session.scanning = true;
      session.sourcesOpen = false;
      for (const t of session.targets) {
        session.scan[t.key] = {
          phase: "scanning",
          message: "Starting…",
          done: 0,
          total: null,
        };
      }
      await startScan(session.targets);
    } catch (e) {
      session.scanning = false;
      console.error("scan failed to start:", e);
    } finally {
      starting = false;
    }
  }

  async function addImage() {
    const picked = await openFileDialog({
      multiple: false,
      directory: false,
      title: "Add a disk image",
      filters: [
        { name: "Disk images", extensions: ["img", "dd", "raw", "001", "e01", "bin"] },
        { name: "All files", extensions: ["*"] },
      ],
    });
    if (typeof picked === "string") {
      const label = picked.split(/[\\/]/).pop() ?? picked;
      addTarget({ kind: "image", key: picked, label });
    }
  }

  /** Close the popup on any click outside it. */
  function onWindowMousedown(e: MouseEvent) {
    if (session.sourcesOpen && popup && !popup.contains(e.target as Node)) {
      session.sourcesOpen = false;
    }
  }

  const summary = $derived(
    session.targets.length === 0
      ? "No sources"
      : session.targets.map((t) => t.label).join(", "),
  );

  const available = $derived(
    session.volumes.filter((v) => !session.targets.some((t) => t.key === v.name)),
  );
</script>

<svelte:window onmousedown={onWindowMousedown} />

<div class="controls" bind:this={popup}>
  <button
    class="sources"
    title="Choose what to scan"
    onclick={() => (session.sourcesOpen = !session.sourcesOpen)}
  >
    <Icon name="hdd-stack" />
    <span class="summary">{summary}</span>
    <Icon name="chevron-down" size={12} />
  </button>

  {#if session.scanning}
    <button class="btn danger" onclick={() => void cancelScan()}>Cancel</button>
  {:else}
    <button
      class="btn primary"
      disabled={session.targets.length === 0}
      title="Scan the listed sources (F5)"
      onclick={() => void scan()}
    >
      Scan
    </button>
  {/if}

  {#if session.sourcesOpen}
    <div class="popup" role="menu">
      <div class="section">
        <div class="heading">Scan list</div>
        {#if session.targets.length === 0}
          <div class="hint">Nothing yet — add a volume below.</div>
        {/if}
        {#each session.targets as t (t.key)}
          <div class="entry">
            <Icon name={t.kind === "image" ? "file-earmark-binary" : "device-hdd"} />
            <span class="label" title={t.key}>{t.label}</span>
            <button
              class="mini"
              title="Remove from scan list"
              disabled={session.scanning}
              onclick={() => removeTarget(t.key)}
            >
              <Icon name="x-lg" size={12} />
            </button>
          </div>
        {/each}
      </div>

      <div class="section">
        <div class="heading">Volumes</div>
        {#each available as v (v.name)}
          <div class="entry" class:disabled={!v.raw_scannable}>
            <Icon name="device-hdd" />
            <span class="label">
              {v.name}
              <span class="detail">
                {v.label ?? v.filesystem} · {formatSize(v.free_bytes)} free of {formatSize(v.total_bytes)}
              </span>
            </span>
            <button
              class="mini"
              title={v.raw_scannable
                ? "Add to scan list"
                : `${v.filesystem} is not scannable yet (directory-walk scanner arrives in M7)`}
              disabled={!v.raw_scannable || session.scanning}
              onclick={() => addTarget({ kind: "volume", key: v.name, label: v.name })}
            >
              <Icon name="plus-lg" size={12} />
            </button>
          </div>
        {/each}
      </div>

      <button class="add-image" disabled={session.scanning} onclick={() => void addImage()}>
        <Icon name="folder2-open" />
        Add disk image…
      </button>
    </div>
  {/if}
</div>

<style>
  .controls {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .sources {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: var(--button-height);
    max-width: 320px;
    padding: 0 var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .sources:hover {
    background: var(--surface-hover);
  }
  .summary {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
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
  .btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--text-on-accent);
    font-weight: var(--font-weight-semibold);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn.danger {
    border-color: var(--danger);
    color: var(--danger);
  }

  .popup {
    position: absolute;
    top: calc(var(--button-height) + var(--space-1));
    right: 0;
    z-index: 50;
    width: 340px;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-medium);
    background: var(--surface-raised);
    box-shadow: 0 8px 24px var(--overlay);
  }

  .section {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .heading {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
    font-weight: var(--font-weight-semibold);
    text-transform: uppercase;
  }

  .hint {
    color: var(--text-muted);
    font-size: var(--font-size-body);
  }

  .entry {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-small);
    color: var(--text-primary);
    font-size: var(--font-size-body);
  }
  .entry:hover {
    background: var(--surface-hover);
  }
  .entry.disabled {
    opacity: 0.55;
  }
  .entry .label {
    flex: 1;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .entry .detail {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }

  .mini {
    display: grid;
    place-items: center;
    width: 22px;
    height: 22px;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: none;
    color: var(--text-secondary);
    cursor: pointer;
  }
  .mini:hover:not(:disabled) {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .mini:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .add-image {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2);
    border: 1px dashed var(--border-strong);
    border-radius: var(--radius-small);
    background: none;
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .add-image:hover:not(:disabled) {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .add-image:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
