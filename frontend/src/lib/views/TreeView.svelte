<!--
  TreeView.svelte — placeholder for the space-analysis tab.

  The real thing is milestone M3 (roadmap.md): the WizTree-style folder tree
  with inline proportional bars, and the treemap whose squarified layout is
  computed in Rust and shipped as a flat rectangle list. The tab exists now
  so the app's shape is final; this panel is what it replaces.
-->
<script lang="ts">
  // Markup-only for now; the script block keeps svelte-check's TS typing of
  // the module intact.
</script>

<div class="placeholder">
  <div class="art">▦</div>
  <h2>Tree view is on its way</h2>
  <p>
    The folder tree and treemap land in the next milestone (M3, v0.3.0):
    expandable size-sorted folders with proportional bars, and a WizTree-style
    treemap of the whole volume — layout computed in Rust, drawn here.
  </p>
</div>

<style>
  .placeholder {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
    text-align: center;
    padding: var(--space-5);
  }
  .art {
    font-size: 64px;
    line-height: 1;
    color: var(--text-muted);
  }
  h2 {
    margin: 0;
    color: var(--text-primary);
    font-size: var(--font-size-heading);
    font-weight: var(--font-weight-semibold);
  }
  p {
    margin: 0;
    max-width: 480px;
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
</style>
