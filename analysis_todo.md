# analysis_todo

Per STANDARDS sec 5.4: freeform list of features, gaps, and ideas. Lower ceremony
than GitHub issues. Rewrite freely. Promote real plans to issues; delete what
goes stale.

## M2 verification (needs a playtest / elevated run)

- [ ] Playtest the app: scan from an elevated launch, type into search on a
      full C: index, confirm keystroke-to-results feels instant and the list
      scrolls smoothly (M2 exit criteria: results within a frame, no
      full-set transfer, sort doesn't block the UI, Cancel visibly works).
- [ ] Screenshot for the README once the UI has real data in it.

## M2 gaps / follow-ups (small, known)

- [ ] Keyboard range selection in the list (Shift+Arrows) - clicks and
      Ctrl/Shift-click are in; features.md sec 3 also wants keyboard extension.
- [ ] Search-the-full-path toggle (features.md sec 2 `[new]`); matching runs on
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
- Scan sources = managed list in a popup (+/x model, all native volumes
  offered, only C: auto-added; "Add disk image..." via the native file
  dialog feeds `scan_image`).
- Two tabs: List (search) and Tree view; Tree view ships in M3 and is a
  placeholder until then.
- Right-click on a row should open the real Explorer context menu
  (`IContextMenu`) - M4 scope, noted in the README.

## M3 verification (needs a playtest)

- [ ] Playtest the Tree view on a real C: index. Exit criteria to eyeball:
      treemap relayout feels instant at any drill (core budget is 100 ms),
      expanding a 100k-child folder doesn't stall, tree totals agree with
      the treemap and WizTree, free space + files ~ volume capacity.
- [ ] Update the README screenshot once both tabs have real data.

## M3 gaps / follow-ups (small, known)

- [ ] In-place filter box for the folder tree (features.md sec 4.1) - needs a
      core subtree-filter; deferred, list search covers the need meanwhile.
- [ ] Treemap right-click context menu (features.md sec 4.2) waits for the M4
      file-operations work it would invoke.
- [ ] Selection sync into the *List* tab (tree/map -> list) - the tree and
      map sync both ways today; the list keeps positional selection.
- [ ] Treemap labels use a fixed 11px Inter; consider a token.

## M4 and later

- [ ] File operations, native `IContextMenu`, index updates after delete,
      CSV/JSON export, row detail pane (surface `VolumeScanOutcome::ads`
      there - collected since M1, still undisplayed).
- [ ] Replace the placeholder app icon with real EmFit artwork
      (`app-icon.svg` at the repo root, then `npx tauri icon ./app-icon.svg`).

## Tree view direction settled with the user (2026-07-26, round 2)

- Scan chips show a real progress bar (native `<progress>`): indeterminate
  until the `$MFT` bitmap yields the expected count, determinate after.
- Treemap paints WizTree's recovered grammar (logs/wiztree_treemap_algorithm.md):
  files = solid boxes with a darkened right/bottom edge bevel, captioned
  `name (size)` over the fill when h > 12px AND w > 80px; labelled folders
  (h > 12, w >= 80) = a reserved 14px header strip (`name\ (999 GiB)`) carved
  off the top *before* squarify, framed by the two-tone 3D frame (0x303030
  left+bottom, 0x404040 right+top). The strip height lives in the *Rust*
  layout (`HEADER_PX` const in treemap.rs); the frontend constant `HEADER_PX`
  in Treemap.svelte must match it.
- Colors, two modes persisted in `config.toml` (`[treemap]`, Settings dialog
  in the native View menu - no on-screen buttons):
  `ranked` (default, WizTree parity) ranks extensions by total allocated
  bytes and assigns the 13-color WizTree palette (FF8514, FFFF07, 30FF45,
  A53FFF, FF4992, 15A3FF, 7F77FF, FF2DED, FF110F, 81AD2E, 15E4B6, BC7829,
  696969) in rank order - unranked extensions and solid folders get the last
  (gray) entry; `extension` colors by extension via the built-in category
  palette for now - TODO(future milestone): user-configurable
  extension->color list (and palette) in settings, persisted in
  `TreemapConfig`. The old `size` bucket mode is gone entirely (no legacy
  config support - pre-release, nothing to migrate).
- Tree-list double-click must NOT re-root the treemap; drilling happens
  only from the map (double-click / breadcrumb).
- Treemap height set by a splitter that applies on drag **release** only.
- Tooltip cannot leave the window: DOM is clipped to the webview. Escaping
  the frame would need a separate borderless always-on-top OS window -
  noted as not worth it for now.

## Notes / decisions worth remembering

- ADS accounting: a stream's **allocated** bytes fold into its owner's
  allocated size; logical size is reported only in the side table, because
  `$BadClus:$Bad` logically spans the whole volume while occupying nothing.
- Sparse streams (fixed 2026-07-27): a sparse attribute with no compression
  unit reports its full *reserved* span as `allocated_size` - `$BadClus`
  showed as a volume-sized file. Sparse streams now sum their run list
  (holes = 0); multi-fragment sparse attrs are undercounted, never over.
- Multi-volume parallelism follows physical disks (fixed 2026-07-27): the
  user's C: and D: are two partitions of one SSD, and scanning them at once
  dropped D: from 292 -> 111 MiB/s and made both finish later. `scan_volumes`
  now lanes targets by `disk_number` - parallel across disks, sequential
  within one. BenchLog caught it.
- Treemap perf lessons: never call a hover-reading fn synchronously inside
  the scene effect (Svelte tracks the whole call tree - defer via rAF);
  keep hot-loop data out of `$state` proxies (`$state.raw` + palette
  snapshot); scene on an offscreen layer, hover = blit + outlines.
- The free-space row is synthetic (`EntryFlags::SYNTHETIC`), parented to the
  root, pushed by `service::scan` - scanners never see it.
- Display collation (sort) deliberately uses the simple fold, not per-volume
  `$UpCase` - one list mixing two volumes needs one consistent order;
  `$UpCase` governs *matching* only.
- The virtualized list is hand-rolled (comment in `FileList.svelte`): the
  row source is a Rust-side window API, which JS virtual-list libraries
  (client-side arrays) don't fit. Above ~1M rows it switches from exact to
  proportional scroll mapping to stay under the browser element-height cap.
- `examples/mft.rs` predates the CLI and overlaps `emfit-cli read-mft`/`scan`;
  fold what's still unique (the two-route extent-map comparison, the size
  breakdown by kind) into `emfit-cli diag` in M6 and delete the example.
