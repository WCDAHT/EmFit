<!--
  UpdateDialog.svelte - Help > Check for updates.

  One dialog covering the whole flow: checking, what was found, downloading,
  installing, and restarting. It never acts on its own - opening it starts the
  check because opening it IS the request, and nothing after that happens
  without another click.

  EmFit ships as a portable executable, so Download installs by replacing the
  running file and then offers a restart. When the install directory will not
  take a write, the download stays where it landed and the dialog says why in
  a sentence the core wrote (`service::update::apply::Blocker`), with the
  by-hand buttons as the fallback. Nothing fails quietly: the same reason is
  in the log file.

  The release notes come off the network and are rendered as TEXT, never with
  `{@html}` (STANDARDS sec 3.6).
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import {
    applyUpdate,
    cancelUpdateDownload,
    checkForUpdate,
    downloadUpdate,
    launchUpdate,
    restartForUpdate,
    revealUpdate,
    skipUpdateVersion,
  } from "../ipc";
  import type { UpdateProgressEvent, UpdateStatus } from "../types";

  interface Props {
    open: boolean;
    onClose: () => void;
    /** A status the caller already has, from the startup check. Skips the
     *  round trip that opening would otherwise make. */
    seeded?: UpdateStatus | null;
  }
  let { open, onClose, seeded = null }: Props = $props();

  type Phase =
    | "checking"
    | "checked"
    | "downloading"
    | "installing"
    /** Replaced; a restart is all that is left. */
    | "installed"
    /** Downloaded but not installed, with a reason. */
    | "blocked"
    | "failed";

  let phase = $state<Phase>("checking");
  let status = $state<UpdateStatus | null>(null);
  let error = $state("");
  let downloaded = $state("");
  let blockedReason = $state("");
  let progress = $state<UpdateProgressEvent | null>(null);
  let unlisten: UnlistenFn | null = null;

  $effect(() => {
    if (!open) return;
    progress = null;
    downloaded = "";
    blockedReason = "";
    error = "";
    if (seeded) {
      status = seeded;
      phase = "checked";
    } else {
      void check();
    }
  });

  // Subscribed once, not per open: the event only fires while a download this
  // dialog started is running, and re-subscribing on every open races the
  // promise that hands back the unlisten function.
  onMount(() => {
    void listen<UpdateProgressEvent>("update:progress", ({ payload }) => {
      progress = payload;
    }).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  });

  async function check() {
    phase = "checking";
    error = "";
    try {
      status = await checkForUpdate();
      phase = "checked";
    } catch (e) {
      error = `${e}`;
      phase = "failed";
    }
  }

  /** Download, then install, with no click in between: the user asked for the
   *  update, not for a file. Installing is what fails gracefully, not the
   *  chain - a blocked install still leaves them the download. */
  async function onDownload() {
    phase = "downloading";
    progress = null;
    blockedReason = "";
    error = "";
    try {
      const result = await downloadUpdate();
      downloaded = result.file_name;
    } catch (e) {
      // A cancel resolves through the same path as a failure; the dialog just
      // returns to the offer rather than showing the user their own click as
      // an error.
      const message = `${e}`;
      if (message.includes("cancelled")) {
        phase = "checked";
      } else {
        error = message;
        phase = "failed";
      }
      return;
    }

    phase = "installing";
    try {
      const applied = await applyUpdate();
      if (applied.replaced) {
        phase = "installed";
      } else {
        blockedReason = applied.reason;
        phase = "blocked";
      }
    } catch (e) {
      // The install could not even be attempted. The file is still downloaded,
      // so offer the same by-hand route a blocked install gets.
      blockedReason = `${e}`;
      phase = "blocked";
    }
  }

  async function onSkip() {
    if (status?.latest) await skipUpdateVersion(status.latest);
    onClose();
  }

  /** The manifest's date, verbatim if it is not a date we can read. */
  function when(raw: string): string {
    if (raw === "") return "";
    const at = new Date(raw);
    return Number.isNaN(at.getTime()) ? raw : at.toLocaleDateString();
  }

  const percent = $derived(
    progress && progress.total ? Math.round((progress.done / progress.total) * 100) : null,
  );

  /** Phases where walking away would interrupt something in flight. */
  const busy = $derived(phase === "downloading" || phase === "installing");

  function onKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    if (phase === "downloading") void cancelUpdateDownload();
    else if (phase !== "installing") onClose();
  }
</script>

<svelte:window onkeydown={open ? onKeydown : undefined} />

{#if open}
  <div
    class="backdrop"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget && !busy) onClose();
    }}
  >
    <div class="dialog" role="dialog" aria-modal="true" aria-label="Check for updates">
      <header>
        <h2>Updates</h2>
        <button class="close" onclick={onClose} aria-label="Close">&times;</button>
      </header>

      {#if phase === "checking"}
        <p class="lead">Checking for a newer version...</p>
      {:else if phase === "failed"}
        <p class="lead">Could not reach the update site.</p>
        <p class="muted detail">{error}</p>
      {:else if status?.state === "available"}
        <p class="lead">
          Version {status.latest} is available.
          {#if status.released_at}
            <span class="muted">Released {when(status.released_at)}.</span>
          {/if}
        </p>
        <p class="muted">You are running {status.current}.</p>

        {#if status.notes}
          <!-- Remote text. Rendered as text on purpose - see the header note. -->
          <pre class="notes">{status.notes}</pre>
        {/if}

        {#if phase === "downloading"}
          <div class="progress">
            <progress max="100" value={percent ?? undefined}></progress>
            <span class="muted">{progress?.display ?? "Starting..."}</span>
          </div>
        {:else if phase === "installing"}
          <div class="progress">
            <progress></progress>
            <span class="muted">Installing...</span>
          </div>
        {:else if phase === "installed"}
          <p class="lead done">Version {status.latest} is installed.</p>
          <p class="muted">
            Restart to start using it. The version you were running is removed
            the next time EmFit opens.
          </p>
        {:else if phase === "blocked"}
          <p class="lead warn">Downloaded, but not installed.</p>
          <p class="reason">{blockedReason}</p>
          <p class="muted">
            {downloaded} is saved in EmFit's app data folder. EmFit runs from a
            single file, so replacing your copy with it finishes the update.
          </p>
        {/if}
      {:else if status?.state === "ahead"}
        <p class="lead">You are running {status.current}, newer than the published {status.latest}.</p>
      {:else if status?.state === "not_listed"}
        <p class="lead">The update site does not list EmFit yet.</p>
        <p class="muted">You are running {status.current}.</p>
      {:else if status}
        <p class="lead">EmFit is up to date.</p>
        <p class="muted">Version {status.current} is the latest published.</p>
      {/if}

      <footer>
        {#if phase === "downloading"}
          <button class="ghost" onclick={() => cancelUpdateDownload()}>Cancel</button>
        {:else if phase === "installing"}
          <!-- The swap is a rename and a copy; there is no safe halfway to
               stop at, and it takes about as long as one file copy. -->
        {:else if phase === "installed"}
          <button class="ghost" onclick={onClose}>Later</button>
          <button class="primary" onclick={() => restartForUpdate()}>Restart now</button>
        {:else if phase === "blocked"}
          <button class="ghost" onclick={() => revealUpdate()}>Show the file</button>
          <button class="primary" onclick={() => launchUpdate()}>Run it</button>
        {:else if phase === "failed"}
          <button class="ghost" onclick={onClose}>Close</button>
          <button class="primary" onclick={check}>Try again</button>
        {:else if status?.state === "available"}
          <button class="ghost" onclick={onSkip}>Skip this version</button>
          <button class="primary" onclick={onDownload}>Download and install</button>
        {:else if phase !== "checking"}
          <button class="primary" onclick={onClose}>Close</button>
        {/if}
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
    width: min(520px, 90vw);
    max-height: 85vh;
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

  .lead {
    margin: 0;
    color: var(--text-primary);
    font-size: var(--font-size-body-lg);
  }
  .lead.done {
    color: var(--positive);
  }
  .lead.warn {
    color: var(--warning);
  }

  /* The core wrote this sentence and it is the whole point of the state -
     give it more weight than the surrounding notes. */
  .reason {
    margin: 0;
    color: var(--text-primary);
    font-size: var(--font-size-body);
    user-select: text;
  }

  .muted {
    margin: 0;
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }
  .detail {
    font-family: var(--font-family-mono);
    user-select: text;
    word-break: break-word;
  }

  .notes {
    margin: 0;
    flex: 1;
    min-height: 0;
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

  .progress {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .progress progress {
    width: 100%;
    height: var(--space-2);
  }

  footer {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  footer button {
    height: var(--button-height);
    padding: 0 var(--space-4);
    border-radius: var(--radius-small);
    font-size: var(--font-size-body);
    cursor: pointer;
  }

  .ghost {
    border: 1px solid var(--border);
    background: var(--surface);
    color: var(--text-secondary);
  }
  .ghost:hover {
    color: var(--text-primary);
    border-color: var(--border-strong);
  }

  .primary {
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--text-on-accent);
  }
  .primary:hover {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
</style>
