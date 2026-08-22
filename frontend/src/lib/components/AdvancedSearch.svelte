<!--
  AdvancedSearch.svelte - the Advanced Search dialog (advanced-search.md C5).

  A form over the query syntax: the panels below fill in fields, this shell
  turns them into a query and writes it into the search box. Nothing here
  filters anything - whatever the dialog can express, you could have typed.

  Two rules the whole feature is built on:
  - Every control explains itself on hover, via `use:tip` (lib/tooltip.ts).
  - Anything the panels do not understand is preserved verbatim, so opening
    the dialog on a hand-written query and pressing Search never loses it.
-->
<script lang="ts">
  import { advanced, buildQuery, load, reset } from "../advanced.svelte";
  import { queryChanged, session } from "../session.svelte";
  import { tip } from "../tooltip";
  import NamesPanel from "./NamesPanel.svelte";

  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open, onClose }: Props = $props();

  let wasOpen = false;

  // Opening reads the search box, so the dialog always starts from what is
  // actually being searched rather than from whatever it held last time.
  $effect(() => {
    if (open && !wasOpen) load(session.text);
    wasOpen = open;
  });

  const preview = $derived(buildQuery());

  function search() {
    session.text = preview;
    queryChanged(true);
    onClose();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") onClose();
    // Enter searches, unless the caret is in a field that wants it.
    if (e.key === "Enter" && !(e.target instanceof HTMLTextAreaElement)) search();
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
    <div class="dialog" role="dialog" aria-modal="true" aria-label="Advanced search">
      <header>
        <h2>Advanced search</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      <div class="panels">
        <NamesPanel />
        <!-- More panels land here, one per commit (advanced-search.md C7 onward). -->

        {#if advanced.rest !== ""}
          <section class="preserved">
            <h3>Kept from the search box</h3>
            <p use:tip={"Terms this dialog has no control for. They are carried through unchanged."}>
              {advanced.rest}
            </p>
          </section>
        {/if}
      </div>

      <footer>
        <div class="preview" use:tip={"The query this dialog will put in the search box."}>
          <span class="label">Query</span>
          <code>{preview || "(everything)"}</code>
        </div>
        <div class="actions">
          <button use:tip={"Clear every field in this dialog."} onclick={reset}>Reset</button>
          <button use:tip={"Close without changing the search."} onclick={onClose}>Cancel</button>
          <button
            class="primary"
            use:tip={"Put this query in the search box and run it."}
            onclick={search}>Search</button
          >
        </div>
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
    /* Resizable: the panels are long, and how much of them a user wants on
       screen at once is their call. */
    resize: both;
    overflow: hidden;
    width: min(760px, 92vw);
    height: min(680px, 88vh);
    min-width: 420px;
    min-height: 280px;
    max-width: 96vw;
    max-height: 92vh;
    display: flex;
    flex-direction: column;
    padding: var(--space-5);
    gap: var(--space-3);
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

  .panels {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    overflow-y: auto;
    padding-right: var(--space-2);
  }

  .preserved h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--font-size-body);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }
  .preserved p {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
    color: var(--text-secondary);
    font-family: var(--font-family-mono);
    font-size: var(--font-size-caption);
    overflow-x: auto;
    white-space: nowrap;
  }

  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding-top: var(--space-3);
    border-top: 1px solid var(--border-subtle);
  }

  .preview {
    flex: 1;
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-width: 0;
  }
  .preview .label {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .preview code {
    flex: 1;
    min-width: 0;
    color: var(--text-secondary);
    font-family: var(--font-family-mono);
    font-size: var(--font-size-caption);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .actions {
    display: flex;
    gap: var(--space-2);
  }
  .actions button {
    height: var(--button-height);
    padding: 0 var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .actions button:hover {
    background: var(--surface-hover);
  }
  .actions .primary {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--text-on-accent);
  }
  .actions .primary:hover {
    background: var(--accent-hover);
  }
</style>
