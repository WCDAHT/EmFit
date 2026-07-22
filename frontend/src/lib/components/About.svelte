<!--
  About.svelte — about/licenses dialog.

  Unlike the Slint template (which embedded license text in the Rust binary
  via include_str! and pushed it through a bridge), the About panel is pure
  frontend now. The license files are imported as raw strings at build time
  with Vite's `?raw` suffix, so they ship inside the webview bundle and
  satisfy the "must contain this license" obligations (SIL OFL, MIT). No Rust
  command is involved. See STANDARDS §5.6.

  Tauri (MIT/Apache-2.0) imposes NO attribution requirement — there is no
  "Made with Slint"-style badge to carry anymore. This panel exists for the
  bundled font and icon licenses plus the app's own proprietary notice.
-->
<script lang="ts">
  import appLicense from "../../../../LICENSE.txt?raw";
  import interLicense from "../../assets/fonts/Inter-OFL.txt?raw";
  import thirdPartyLicenses from "../../assets/generated-licenses.txt?raw";


  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open, onClose }: Props = $props();

  const licenses = [
    { name: "app-name (this software)", body: appLicense },
    { name: "Inter font (SIL OFL 1.1)", body: interLicense },
    { name: "Third Party", body: thirdPartyLicenses },
  ];

  let selected = $state(0);

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") onClose();
  }
</script>

<svelte:window onkeydown={open ? onKeydown : undefined} />

{#if open}
  <!-- Backdrop. Clicking it (but not the dialog) closes. -->
  <div
    class="backdrop"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) onClose();
    }}
  >
    <div class="dialog" role="dialog" aria-modal="true" aria-label="About app-name">
      <header>
        <h2>About app-name</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      <p class="muted">
        Copyright 2026 Steven R. Schiavone. Proprietary; all rights reserved.
      </p>
      <p class="muted">
        Built with Tauri and Svelte. Bundles Inter (SIL OFL 1.1) and Bootstrap
        Icons (MIT). Full license texts below.
      </p>

      <nav class="tabs">
        {#each licenses as license, i}
          <button
            class="tab"
            class:active={i === selected}
            onclick={() => (selected = i)}
          >
            {license.name}
          </button>
        {/each}
      </nav>

      <!-- read-only, but selectable so a clause can be copied for legal/casefile. -->
      <pre class="body">{licenses[selected].body}</pre>
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
    width: min(720px, 90vw);
    height: min(560px, 85vh);
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

  .muted {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }

  .tabs {
    display: flex;
    gap: var(--space-1);
  }

  .tab {
    flex: 1;
    height: var(--button-height);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
  .tab.active {
    border-color: var(--accent);
    color: var(--text-primary);
  }

  .body {
    flex: 1;
    margin: 0;
    overflow: auto;
    padding: var(--space-3);
    background: var(--surface-sunken);
    border-radius: var(--radius-small);
    color: var(--text-secondary);
    font-family: var(--font-family-mono);
    font-size: var(--font-size-caption);
    white-space: pre-wrap;
    user-select: text;
  }
</style>
