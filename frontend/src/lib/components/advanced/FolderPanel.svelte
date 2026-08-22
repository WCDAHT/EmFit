<!--
  FolderPanel.svelte - what a folder holds (advanced-search.md C12).

  Every condition here is about a folder's direct contents, not its subtree:
  a folder with three files in it has childfilecount:3 however many are
  nested below.
-->
<script lang="ts">
  import { advanced, type CountFilter } from "../../advanced.svelte";
  import { tip } from "../../tooltip";

  const TOGGLES = [
    {
      key: "matchCase" as const,
      label: "Match case",
      hint: "Uppercase and lowercase count as different letters. Writes: case:child:",
    },
    {
      key: "wholeWords" as const,
      label: "Match whole words",
      hint: "The text has to be a whole word in the child's name. Writes: ww:child:",
    },
    {
      key: "diacritics" as const,
      label: "Match diacritics",
      hint: "Accented letters stop matching their plain forms. Writes: diacritics:child:",
    },
  ];
</script>

{#snippet countRow(label: string, filter: CountFilter, hint: string)}
  <div class="row">
    <span class="label">{label}</span>
    <div class="bounds" use:tip={hint}>
      <input
        bind:value={filter.from}
        placeholder="from"
        inputmode="numeric"
        spellcheck="false"
        aria-label={`${label} from`}
      />
      <span class="to">to</span>
      <input
        bind:value={filter.to}
        placeholder="to"
        inputmode="numeric"
        spellcheck="false"
        aria-label={`${label} to`}
      />
    </div>
  </div>
{/snippet}

<section>
  <h3>Folders with a child subfolder or file name containing:</h3>
  <input
    bind:value={advanced.child.text}
    use:tip={"Match the folder, not the child. Writes: child:invoice"}
    placeholder="invoice"
    spellcheck="false"
    aria-label="Child name contains"
  />
  <div class="toggles">
    {#each TOGGLES as t (t.key)}
      <label use:tip={t.hint}>
        <input type="checkbox" bind:checked={advanced.child[t.key]} />
        <span>{t.label}</span>
      </label>
    {/each}
  </div>

  {@render countRow(
    "Folders with this many subfolders and files:",
    advanced.childCount,
    "Everything directly inside the folder, files and subfolders together. Writes: childcount:2..8",
  )}
  {@render countRow(
    "Folders with this many files:",
    advanced.childFileCount,
    "Files directly inside the folder. Writes: childfilecount:2..8",
  )}
  {@render countRow(
    "Folders with this many subfolders:",
    advanced.childFolderCount,
    "Subfolders directly inside the folder. Writes: childfoldercount:2..8",
  )}
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  h3 {
    margin: 0;
    font-size: var(--font-size-body);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }

  .row {
    display: grid;
    grid-template-columns: 280px 1fr;
    align-items: center;
    gap: var(--space-3);
  }

  .label {
    color: var(--text-secondary);
    font-size: var(--font-size-body);
    text-align: right;
  }

  .bounds {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .bounds input {
    flex: 1;
    max-width: 120px;
  }

  .to {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }

  input:not([type="checkbox"]) {
    min-width: 0;
    height: var(--input-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  input:focus {
    border-color: var(--selection);
  }

  .toggles {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
    margin-bottom: var(--space-2);
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
