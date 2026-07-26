<!--
  ScanStatus.svelte — the elevation banner and per-target scan status chips.
  Renders nothing when there is nothing to say, so the layout stays flat.
-->
<script lang="ts">
  import { relaunchElevated } from "../ipc";
  import { session } from "../session.svelte";

  function chipText(key: string): string {
    const s = session.scan[key];
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

  const entries = $derived(
    session.targets.filter((t) => session.scan[t.key] !== undefined),
  );
</script>

{#if !session.elevated}
  <div class="banner">
    <span>Not running as Administrator — raw volume scans will be refused.</span>
    <button onclick={() => void relaunchElevated()}>Relaunch elevated</button>
  </div>
{/if}

{#if entries.length > 0}
  <div class="chips">
    {#each entries as t (t.key)}
      {@const s = session.scan[t.key]}
      <span class="chip" class:error={s.phase === "error"} class:done={s.phase === "done"}>
        <span class="name">{t.label}</span>
        {chipText(t.key)}
      </span>
    {/each}
  </div>
{/if}

<style>
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
  .banner button {
    height: 24px;
    padding: 0 var(--space-2);
    border: 1px solid var(--warning);
    border-radius: var(--radius-small);
    background: none;
    color: var(--warning);
    font-family: inherit;
    font-size: var(--font-size-caption);
    cursor: pointer;
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }

  .chip {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }
  .chip .name {
    color: var(--text-primary);
    font-weight: var(--font-weight-semibold);
  }
  .chip.error {
    border-color: var(--danger);
    color: var(--danger);
  }
  .chip.done {
    color: var(--positive);
  }
</style>
