<!--
  StatusBar.svelte — object count, selection count and total, volume total
  (features.md §3). The selection total is the number users select things to
  learn: "how much would deleting this free up".
-->
<script lang="ts">
  import { session, selectionCount } from "../session.svelte";

  const objects = $derived(session.total.toLocaleString());
  const selected = $derived(selectionCount());
</script>

<div class="status">
  <span>{objects} objects</span>
  {#if selected > 0}
    <span class="sep">·</span>
    <span>
      {selected.toLocaleString()} selected{#if session.summary}
        &nbsp;({session.summary.bytes_display}, {session.summary.allocated_display} on disk){/if}
    </span>
  {/if}
  <span class="spacer"></span>
  {#if session.volumesTotalDisplay}
    <span>{session.volumesTotalDisplay} indexed</span>
  {/if}
</div>

<style>
  .status {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-top: 1px solid var(--border);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }
  .sep {
    color: var(--text-muted);
  }
  .spacer {
    flex: 1;
  }
</style>
