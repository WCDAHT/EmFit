<!--
  NamesPanel.svelte - "File names containing..." (advanced-search.md C6).

  Four ways to say what a name must contain, each with the three match
  toggles. They map onto the query grammar directly: words are ANDed by the
  space between them, a phrase is quoted, any-of is an OR group, and none-of
  is a run of negated terms. Every control says which syntax it writes, so
  the dialog teaches the language it is a front end for.
-->
<script lang="ts">
  import { advanced, type NameField } from "../advanced.svelte";
  import { tip } from "../tooltip";

  const TOGGLES: { key: keyof Omit<NameField, "text">; label: string; hint: string }[] = [
    {
      key: "matchCase",
      label: "Match case",
      hint: "Uppercase and lowercase count as different letters. Writes: case:",
    },
    {
      key: "wholeWords",
      label: "Match whole words",
      hint: "The text has to be a whole word, not part of a longer one. Writes: ww:",
    },
    {
      key: "diacritics",
      label: "Match diacritics",
      hint: "Accented letters stop matching their plain forms. Writes: diacritics:",
    },
  ];
</script>

{#snippet row(field: NameField, label: string, hint: string, placeholder: string)}
  <div class="field">
    <span class="label">{label}</span>
    <input
      bind:value={field.text}
      use:tip={hint}
      {placeholder}
      spellcheck="false"
      aria-label={label}
    />
    <div class="toggles">
      {#each TOGGLES as t (t.key)}
        <label use:tip={t.hint}>
          <input type="checkbox" bind:checked={field[t.key]} />
          <span>{t.label}</span>
        </label>
      {/each}
    </div>
  </div>
{/snippet}

<section>
  <h3>File names containing...</h3>

  {@render row(
    advanced.all,
    "all these words",
    "Every word must appear somewhere in the name, in any order. Writes each word as its own term: annual report",
    "annual report",
  )}
  {@render row(
    advanced.phrase,
    "this exact phrase",
    'The name must contain this exact text, spaces and all. Writes: "annual report"',
    "annual report",
  )}
  {@render row(
    advanced.any,
    "any of these words",
    "At least one of these words must appear. Writes an OR group: <annual | report>",
    "annual report",
  )}
  {@render row(
    advanced.none,
    "none of these words",
    "Names containing any of these words are excluded. Writes: !draft",
    "draft copy",
  )}
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  h3 {
    margin: 0;
    font-size: var(--font-size-body);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }

  .field {
    display: grid;
    grid-template-columns: 140px 1fr;
    align-items: center;
    gap: var(--space-2) var(--space-3);
  }

  .label {
    color: var(--text-secondary);
    font-size: var(--font-size-body);
    text-align: right;
  }

  .field input:not([type="checkbox"]) {
    height: var(--input-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    min-width: 0;
  }
  .field input:focus {
    border-color: var(--selection);
  }

  .toggles {
    grid-column: 2;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
  }
  .toggles label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
</style>
