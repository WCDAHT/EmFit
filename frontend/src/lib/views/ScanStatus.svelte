<!--
  ScanStatus.svelte - the elevation banner and per-target scan status chips.
  Renders nothing when there is nothing to say, so the layout stays flat.
-->
<script lang="ts">
  import { relaunchElevated } from "../ipc";
  import { session } from "../session.svelte";

  let relaunching = $state(false);

  /** Hand over to an elevated instance. On success this window is gone before
   *  the await resolves - the shell command exits the process once the launch
   *  is through. Only a refusal comes back here. */
  async function relaunch() {
    relaunching = true;
    try {
      await relaunchElevated();
    } catch (e) {
      // Almost always a dismissed UAC prompt, which is a decision, not a
      // fault: stay unelevated and leave the banner up.
      console.warn(`elevated relaunch did not happen: ${e}`);
      relaunching = false;
    }
  }

  function chipText(key: string): string {
    const s = session.scan[key];
    if (!s) return "";
    switch (s.phase) {
      case "scanning":
        if (s.total !== null && s.total > 0 && s.done > 0) {
          return `${s.message} - ${s.done.toLocaleString()} / ${s.total.toLocaleString()}`;
        }
        return s.done > 0 ? `${s.message} - ${s.done.toLocaleString()}` : s.message;
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
    <span>Not running as Administrator - raw volume scans will be refused.</span>
    <button onclick={() => void relaunch()} disabled={relaunching}>
      {relaunching ? "Waiting for Windows..." : "Relaunch elevated"}
    </button>
  </div>
{/if}

{#if entries.length > 0}
  <div class="chips">
    {#each entries as t (t.key)}
      {@const s = session.scan[t.key]}
      <span class="chip" class:error={s.phase === "error"} class:done={s.phase === "done"}>
        <span class="name">{t.label}</span>
        {#if s.phase === "scanning"}
          <!-- Native <progress>: with a total it is determinate; without a
               value it renders the indeterminate animation - exactly the
               "indeterminate until the file count is known" behavior. -->
          {#if s.total !== null && s.total > 0}
            <progress max={s.total} value={Math.min(s.done, s.total)}></progress>
          {:else}
            <progress></progress>
          {/if}
        {/if}
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
  .chip progress {
    width: 120px;
    height: 8px;
  }
  .chip.error {
    border-color: var(--danger);
    color: var(--danger);
  }
  .chip.done {
    color: var(--positive);
  }
</style>
