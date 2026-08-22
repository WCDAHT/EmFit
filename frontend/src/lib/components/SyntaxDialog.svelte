<!--
  SyntaxDialog.svelte - the Search > Search syntax reference.

  The whole query language: operators, wildcards, macros, modifiers, functions,
  and the value grammars. The list comes from Rust (`search_syntax`), generated
  from the parser's own tables, so this dialog cannot advertise something the
  engine would ignore - and there is no second copy of the grammar here to
  drift (STANDARDS sec 6, anti-pattern 5).

  Clicking a row appends it to the search box. Esc closes, like About.svelte.
-->
<script lang="ts">
  import { searchSyntax } from "../ipc";
  import { hooks } from "../session.svelte";
  import type { SyntaxSectionDto } from "../types";

  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open, onClose }: Props = $props();

  let sections: SyntaxSectionDto[] = $state([]);
  let copied = $state("");

  // Fetched on first open, not at startup: nobody pays for a dialog they
  // never open, and the grammar cannot change while the app runs.
  $effect(() => {
    if (open && sections.length === 0) void searchSyntax().then((s) => (sections = s));
  });

  function insert(token: string) {
    hooks.insertSyntax?.(token);
    copied = token;
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
    <div class="dialog" role="dialog" aria-modal="true" aria-label="Search syntax">
      <header>
        <h2>Search syntax</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      <p class="hint">
        Click any entry to add it to the search box.
        {#if copied}<span class="added">Added {copied}</span>{/if}
      </p>

      <div class="body">
        {#each sections as section (section.title)}
          <section>
            <h3>{section.title}</h3>
            <ul>
              {#each section.entries as e (e.token)}
                <li>
                  <button
                    class="token"
                    title={`Insert ${e.token} into the search box`}
                    onclick={() => insert(e.token)}
                  >
                    {e.token.trim() === "" ? "space" : e.token}
                  </button>
                  <span class="summary">{e.summary}</span>
                </li>
              {/each}
            </ul>
          </section>
        {/each}
      </div>
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
    width: min(720px, 92vw);
    max-height: 86vh;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
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

  .hint {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .added {
    margin-left: var(--space-2);
    color: var(--accent);
  }

  .body {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
    gap: var(--space-4);
    overflow-y: auto;
    padding-right: var(--space-2);
  }

  h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--font-size-body);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }

  ul {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
  }

  .token {
    flex: 0 0 auto;
    max-width: 45%;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
    color: var(--text-primary);
    font-family: var(--font-family-mono);
    font-size: var(--font-size-caption);
    text-align: left;
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .token:hover {
    border-color: var(--selection);
    color: var(--accent);
  }

  .summary {
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
  }
</style>
