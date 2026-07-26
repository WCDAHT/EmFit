# analysis_todo

Per STANDARDS §5.4: freeform list of features, gaps, and ideas. Lower ceremony
than GitHub issues. Rewrite freely. Promote real plans to issues; delete what
goes stale.

## M2 verification (needs a playtest / elevated run)

- [ ] Playtest the app: scan from an elevated launch, type into search on a
      full C: index, confirm keystroke-to-results feels instant and the list
      scrolls smoothly (M2 exit criteria: results within a frame, no
      full-set transfer, sort doesn't block the UI, Cancel visibly works).
- [ ] Screenshot for the README once the UI has real data in it.

## M2 gaps / follow-ups (small, known)

- [ ] Keyboard range selection in the list (Shift+Arrows) — clicks and
      Ctrl/Shift-click are in; features.md §3 also wants keyboard extension.
- [ ] Search-the-full-path toggle (features.md §2 `[new]`); matching runs on
      names only today, path scoping via backticks covers the common case.
- [ ] Whole-word and diacritic-insensitive toggles (the `Filters.csv`
      columns are parsed and ignored, as in v1).
- [ ] Search history / saved searches (listed for M6 anyway).
- [ ] `Path` column sort decorates with full path strings (one String per
      hit for the duration of the sort); fine for filtered views, measure on
      a full 5M-row sort before M3.

## UI direction settled with the user (2026-07-26)

- No in-app header; native window menu (File / View / Help) carries About,
  shortcuts, theme, sources, rescan, exit.
- Scan sources = managed list in a popup (＋/× model, all native volumes
  offered, only C: auto-added; "Add disk image…" via the native file
  dialog feeds `scan_image`).
- Two tabs: List (search) and Tree view; Tree view ships in M3 and is a
  placeholder until then.
- Right-click on a row should open the real Explorer context menu
  (`IContextMenu`) — M4 scope, noted in the README.

## M3 and later

- [ ] Folder tree pane, treemap (layout in Rust, flat rectangle list over
      IPC), file-type breakdown, top-N views.
- [ ] Surface `VolumeScanOutcome::ads` in the row detail pane (data is
      collected since M1; nothing displays it yet).
- [ ] Replace the placeholder app icon with real EmFit artwork
      (`app-icon.svg` at the repo root, then `npx tauri icon ./app-icon.svg`).

## Notes / decisions worth remembering

- ADS accounting: a stream's **allocated** bytes fold into its owner's
  allocated size; logical size is reported only in the side table, because
  `$BadClus:$Bad` logically spans the whole volume while occupying nothing.
- The free-space row is synthetic (`EntryFlags::SYNTHETIC`), parented to the
  root, pushed by `service::scan` — scanners never see it.
- Display collation (sort) deliberately uses the simple fold, not per-volume
  `$UpCase` — one list mixing two volumes needs one consistent order;
  `$UpCase` governs *matching* only.
- The virtualized list is hand-rolled (comment in `FileList.svelte`): the
  row source is a Rust-side window API, which JS virtual-list libraries
  (client-side arrays) don't fit. Above ~1M rows it switches from exact to
  proportional scroll mapping to stay under the browser element-height cap.
- `examples/mft.rs` predates the CLI and overlaps `emfit-cli read-mft`/`scan`;
  fold what's still unique (the two-route extent-map comparison, the size
  breakdown by kind) into `emfit-cli diag` in M6 and delete the example.
