<!--
  App.svelte — root view. The Tauri/Svelte successor to main.slint.

  Replace the body with your first real view (put screen-level views under
  src/lib/views/, reusable pieces under src/lib/components/; see STANDARDS
  §3.1). The title bar, theme toggle, and About dialog wiring below are the
  template's worked example of the conventions.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import Icon from "./lib/components/Icon.svelte";
  import About from "./lib/components/About.svelte";
  import ShortcutsDialog from "./lib/components/ShortcutsDialog.svelte";
  import { toggleThemeMode, getThemeMode, syncThemeWithConfig, type ThemeMode } from "./lib/theme";
  import { greet } from "./lib/ipc";

  let theme = $state<ThemeMode>(getThemeMode());
  let aboutOpen = $state(false);
  let shortcutsOpen = $state(false);
  let bridgeResult = $state("");

  // First paint used the localStorage cache; reconcile with the durable theme
  // in the backend config now that async IPC is available (STANDARDS §3.3).
  onMount(async () => {
    theme = await syncThemeWithConfig();
  });

  async function testBridge() {
    // Demonstrates the typed IPC layer calling into the Rust core.
    bridgeResult = await greet("Brunch");
  }

  // All app shortcuts are dispatched here at the root (STANDARDS §3.7), not
  // sprinkled through children. Open dialogs own their own Esc-to-close, so we
  // suppress the global shortcuts while one is up. Add app shortcuts (Ctrl+O,
  // Ctrl+S, …) here as the views that need them land.
  function onKeydown(e: KeyboardEvent) {
    const dialogOpen = aboutOpen || shortcutsOpen;
    if (e.key === "F1") {
      e.preventDefault();
      if (!dialogOpen) shortcutsOpen = true;
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<main>
  <header class="titlebar">
    <h1>app-name</h1>
    <div class="actions">
      <button
        class="icon-btn"
        title="Toggle light/dark"
        onclick={() => (theme = toggleThemeMode())}
      >
        <Icon name={theme === "light" ? "moon" : "sun"} />
      </button>
      <button
        class="icon-btn"
        title="Keyboard shortcuts (F1)"
        onclick={() => (shortcutsOpen = true)}
      >
        <Icon name="keyboard" />
      </button>
      <button class="btn" onclick={() => (aboutOpen = true)}>About</button>
    </div>
  </header>

  <section class="body">
    <p class="hint">
      Replace this view with your first screen. See STANDARDS §3.1.
    </p>

    <button class="btn primary" onclick={testBridge}>Test shell↔core bridge</button>
    {#if bridgeResult}
      <p class="result">{bridgeResult}</p>
    {/if}
  </section>
</main>

<About open={aboutOpen} onClose={() => (aboutOpen = false)} />
<ShortcutsDialog open={shortcutsOpen} onClose={() => (shortcutsOpen = false)} />

<style>
  main {
    height: 100%;
    display: flex;
    flex-direction: column;
    padding: var(--space-5);
    gap: var(--space-3);
  }

  .titlebar {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  h1 {
    margin: 0;
    font-size: var(--font-size-title);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .body {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    align-items: flex-start;
  }

  .hint {
    margin: 0;
    color: var(--text-secondary);
  }

  .result {
    margin: 0;
    color: var(--positive);
  }

  /* Buttons are hand-styled here only to demonstrate the tokens. Real apps
   * should factor a Button.svelte component on the second use (STANDARDS §3.1)
   * and prefer native <button> semantics for focus/keyboard behavior. */
  .btn {
    height: var(--button-height);
    padding: 0 var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .btn:hover {
    background: var(--surface-hover);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--text-on-accent);
    font-weight: var(--font-weight-semibold);
  }
  .btn.primary:hover {
    background: var(--accent-hover);
  }

  .icon-btn {
    display: grid;
    place-items: center;
    width: var(--button-height);
    height: var(--button-height);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .icon-btn:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
</style>
