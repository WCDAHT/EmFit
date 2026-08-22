<!--
  LocatedPanel.svelte - "Located in" (advanced-search.md C7).

  Two ways to name a folder, and they are different searches: with subfolders
  it is the backtick scope, which matches the whole subtree; without, it is
  `infolder:`, which matches that folder's direct contents only.
-->
<script lang="ts">
  import { open as openFolderDialog } from "@tauri-apps/plugin-dialog";
  import { advanced } from "../../advanced.svelte";
  import { tip } from "../../tooltip";

  async function browse() {
    const picked = await openFolderDialog({
      multiple: false,
      directory: true,
      title: "Search in folder",
    });
    if (typeof picked === "string") advanced.located.path = picked;
  }
</script>

<section>
  <h3>Located in:</h3>

  <div class="row">
    <input
      bind:value={advanced.located.path}
      use:tip={"Only search inside this folder. Leave it empty to search every scanned drive."}
      placeholder="C:\Users\Admin\Documents"
      spellcheck="false"
      aria-label="Located in"
    />
    <button use:tip={"Pick the folder instead of typing its path."} onclick={browse}>
      Browse...
    </button>
  </div>

  <label
    use:tip={"On: everything under the folder, written as a `path` scope. Off: only what sits directly in it, written as infolder:"}
  >
    <input type="checkbox" bind:checked={advanced.located.subfolders} />
    <span>Include subfolders</span>
  </label>
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
    display: flex;
    gap: var(--space-2);
  }

  input:not([type="checkbox"]) {
    flex: 1;
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

  button {
    height: var(--input-height);
    padding: 0 var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  button:hover {
    background: var(--surface-hover);
  }

  label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
</style>
