<!--
  TreeView.svelte — the space-analysis tab (roadmap M3): folder tree on top,
  treemap below, file types beside it. Selection is shared through
  `session.focus`: click in the map reveals in the tree and vice versa;
  double-click a directory anywhere to drill the map into it.
-->
<script lang="ts">
  import FolderTree from "./FolderTree.svelte";
  import Treemap from "./Treemap.svelte";
  import TypesPanel from "./TypesPanel.svelte";
  import { session, queryChanged, sortBy } from "../session.svelte";

  /** "Top files" / "Top folders" (features.md §4.4): the List tab already is
   *  that view once kind-filtered and size-sorted — jump it there. */
  function topN(kind: "file" | "folder") {
    session.text = `${kind}:`;
    session.tab = "list";
    if (session.sortKey !== "size" || session.sortAsc) {
      session.sortKey = "path"; // force sortBy to reset to size desc
      sortBy("size");
    }
    queryChanged(true);
  }
</script>

<div class="controls">
  <label>
    Depth
    <select bind:value={session.treemapDepth}>
      {#each [2, 3, 4, 5, 6, 8] as d (d)}
        <option value={d}>{d}</option>
      {/each}
    </select>
  </label>
  <label>
    Color by
    <select bind:value={session.colorMode}>
      <option value="type">File type</option>
      <option value="folder">Folder</option>
    </select>
  </label>
  <label>
    Show
    <select bind:value={session.sizeMode}>
      <option value="allocated">Size on disk</option>
      <option value="logical">Logical size</option>
    </select>
  </label>
  <span class="flex"></span>
  <button class="quick" onclick={() => topN("file")}>Top files</button>
  <button class="quick" onclick={() => topN("folder")}>Top folders</button>
</div>

<FolderTree />

<div class="bottom">
  <Treemap />
  <div class="side">
    <TypesPanel />
  </div>
</div>

<style>
  .controls {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .controls label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
  .controls select {
    height: 24px;
    padding: 0 var(--space-1);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  .flex {
    flex: 1;
  }
  .quick {
    height: 24px;
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
  .quick:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }

  .bottom {
    display: flex;
    gap: var(--space-2);
    flex: 1.2;
    min-height: 0;
  }
  .side {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 0 0 340px;
  }
</style>
