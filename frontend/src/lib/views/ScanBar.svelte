<!--
  ScanBar.svelte — drive selection and scan control (features.md §7).

  One checkbox per volume, a Scan button, a Cancel button that is actually
  wired (v1's flag was connected to nothing), and per-volume status inline —
  one drive failing shows its error here while the others finish. Unscannable
  volumes (FAT32, spanned) stay listed but disabled, with the reason, until
  the directory-walk scanner lands in M7.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { cancelScan, elevationStatus, listVolumes, relaunchElevated, startScan } from "../ipc";
  import { session, hooks } from "../session.svelte";

  let starting = $state(false);

  onMount(async () => {
    hooks.rescan = () => void scan();
    session.elevated = await elevationStatus();
    await refresh();
    // First run: preselect everything scannable, so F5 / Scan just works.
    for (const v of session.volumes) {
      if (v.raw_scannable) session.checked.add(v.name);
    }
  });

  async function refresh() {
    session.volumes = await listVolumes();
  }

  async function scan() {
    const drives = [...session.checked];
    if (drives.length === 0 || session.scanning || starting) return;
    starting = true;
    try {
      session.scanning = true;
      for (const drive of drives) {
        session.scan[drive] = {
          phase: "scanning",
          message: "Starting…",
          done: 0,
          total: null,
        };
      }
      await startScan(drives);
    } catch (e) {
      session.scanning = false;
      console.error("scan failed to start:", e);
    } finally {
      starting = false;
    }
  }

  function progressText(name: string): string {
    const s = session.scan[name];
    if (!s) return "";
    switch (s.phase) {
      case "scanning":
        return s.done > 0 ? `${s.message} — ${s.done.toLocaleString()}` : s.message;
      case "done":
        return s.summary ?? "done";
      case "error":
        return s.error ?? "failed";
    }
  }
</script>

<div class="scanbar">
  {#if !session.elevated}
    <div class="banner">
      <span>
        Not running as Administrator — raw volume scans will be refused.
      </span>
      <button class="btn" onclick={() => void relaunchElevated()}>
        Relaunch elevated
      </button>
    </div>
  {/if}

  <div class="row">
    <div class="volumes">
      {#each session.volumes as v (v.name)}
        <label
          class="volume"
          class:disabled={!v.raw_scannable}
          title={v.raw_scannable
            ? `${v.name} ${v.label ?? ""} — ${v.free_display} free of ${v.total_display}`
            : `${v.filesystem} is not scannable yet (directory-walk scanner arrives in M7)`}
        >
          <input
            type="checkbox"
            disabled={!v.raw_scannable || session.scanning}
            checked={session.checked.has(v.name)}
            onchange={(e) => {
              if (e.currentTarget.checked) session.checked.add(v.name);
              else session.checked.delete(v.name);
            }}
          />
          <span class="name">{v.name}</span>
          <span class="detail">{v.label ?? v.filesystem}</span>
          {#if session.scan[v.name]}
            <span
              class="status"
              class:error={session.scan[v.name].phase === "error"}
              class:done={session.scan[v.name].phase === "done"}
            >
              {progressText(v.name)}
            </span>
          {/if}
        </label>
      {/each}
    </div>

    <div class="actions">
      {#if session.scanning}
        <button class="btn danger" onclick={() => void cancelScan()}>Cancel</button>
      {:else}
        <button
          class="btn primary"
          disabled={session.checked.size === 0}
          title="Scan the selected drives (F5)"
          onclick={() => void scan()}
        >
          Scan
        </button>
      {/if}
    </div>
  </div>
</div>

<style>
  .scanbar {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--warning);
    border-radius: var(--radius-small);
    color: var(--warning);
    font-size: var(--font-size-body);
  }

  .row {
    display: flex;
    align-items: flex-start;
    gap: var(--space-3);
  }

  .volumes {
    flex: 1;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }

  .volume {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .volume.disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .volume .name {
    color: var(--text-primary);
    font-weight: var(--font-weight-semibold);
  }
  .volume .detail {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .volume .status {
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }
  .volume .status.error {
    color: var(--danger);
  }
  .volume .status.done {
    color: var(--positive);
  }

  .actions {
    flex: 0 0 auto;
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
  .btn:hover {
    background: var(--surface-hover);
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
</style>
