# analysis_todo

Per STANDARDS §5.4: freeform list of features, gaps, and ideas. Lower ceremony
than GitHub issues. Rewrite freely. Promote real plans to issues; delete what
goes stale.

## M2 and later

- [ ] Instant name search over the index (substring, then wildcard/regex);
      `$Upcase`-driven case folding.
- [ ] Row-window IPC contract + Tauri command surface (the index never
      crosses the boundary).
- [ ] Directory size rollup views: folder tree pane, treemap, file types.
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
- `examples/mft.rs` predates the CLI and overlaps `emfit-cli read-mft`/`scan`;
  fold what's still unique (the two-route extent-map comparison, the size
  breakdown by kind) into `emfit-cli diag` in M6 and delete the example.
