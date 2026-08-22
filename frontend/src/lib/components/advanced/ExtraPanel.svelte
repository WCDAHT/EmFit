<!--
  ExtraPanel.svelte - additional search options (advanced-search.md C13).

  The escape hatch, and the other half of the dialog's promise that nothing
  typed is lost: terms no panel above understands land here when the dialog
  opens, and whatever is here is appended to the query verbatim on Search.
  Anything the query language can express but this form cannot goes here.
-->
<script lang="ts">
  import { advanced } from "../../advanced.svelte";
  import { tip } from "../../tooltip";
</script>

<section>
  <h3>Additional search options:</h3>
  <input
    bind:value={advanced.rest}
    use:tip={"Query syntax added to the search as written. Terms from the search box that no field above could hold are kept here."}
    placeholder="dm:last7days !`C:\Windows`"
    spellcheck="false"
    aria-label="Additional search options"
  />
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

  input {
    height: var(--input-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: var(--font-family-mono);
    font-size: var(--font-size-caption);
  }
  input:focus {
    border-color: var(--selection);
  }
</style>
