<!--
  ShortcutsDialog.svelte — the F1 "Keyboard shortcuts" help dialog.

  STANDARDS §3.7: every shortcut must be discoverable, both on the control that
  invokes it and in a Keyboard Shortcuts help dialog. This is that dialog. Keep
  the list in step with the handlers wired in App.svelte. Esc closes it (the
  dialog owns its own close key, like About.svelte).

  This ships the standard set as a worked example; trim it to what your app
  actually wires, and add app-specific rows as views land.
-->
<script lang="ts">
  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open, onClose }: Props = $props();

  const SHORTCUTS: { keys: string; action: string }[] = [
    { keys: "Ctrl + F", action: "Focus the search box" },
    { keys: "F5", action: "Rescan the selected drives" },
    { keys: "Esc", action: "Cancel scan / clear search / clear selection" },
    { keys: "Ctrl + A", action: "Select all results" },
    { keys: "Ctrl + Click", action: "Toggle a row in the selection" },
    { keys: "Shift + Click", action: "Select a range of rows" },
    { keys: "F1", action: "Show this shortcuts list" },
  ];

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
    <div class="dialog" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts">
      <header>
        <h2>Keyboard shortcuts</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      <ul class="list">
        {#each SHORTCUTS as s}
          <li class="row">
            <kbd>{s.keys}</kbd>
            <span class="action">{s.action}</span>
          </li>
        {/each}
      </ul>
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
    width: min(420px, 90vw);
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
  .close:hover {
    color: var(--text-primary);
  }

  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  kbd {
    flex: 0 0 auto;
    min-width: var(--space-7);
    padding: var(--space-1) var(--space-2);
    text-align: center;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
    color: var(--text-primary);
    font-family: var(--font-family-mono);
    font-size: var(--font-size-caption);
  }

  .action {
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
</style>
