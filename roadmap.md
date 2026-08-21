# EmFit - Roadmap

Milestones derived from [features.md](./features.md) (what) and
[architecture.md](./architecture.md) (how). Ordered by dependency first, user
value second.

**No dates.** Milestones are sized S / M / L relative to each other, and each
one has exit criteria you can actually check. A milestone is done when its exits
pass, not when its features feel finished.

**NTFS is the priority.** Every milestone up to M7 assumes exactly one scanner.
Additional filesystems land last, as the user chose - with one standing
discipline that keeps that cheap, in sec "The one rule" below.

| | Milestone | Size | Ships |
|---|---|---|---|
| **M0** | Foundation | M | nothing user-visible |
| **M1** | NTFS engine | L | a CLI that scans C: correctly and fast |
| **M2** | Shell, list, search | L | `0.2.0` - the app is usable |
| **M3** | Space analysis | L | `0.3.0` - the app is *the point* |
| **M4** | Operations & export | M | `0.4.0` |
| **M5** | Live updates & persistence | M | `0.5.0` |
| **M6** | Polish & release readiness | M | `0.9.0` -> `1.0.0` |
| **M7** | Additional filesystems | L | `1.1.0`+ |

Version numbers follow STANDARDS sec 5.2: `0.x.y` until there is a real user,
`1.0.0` when the scan file format is stable.

---

## The one rule

Because scanners are deferred to M7, one discipline has to hold from M1 onward,
without exception:

> **The NTFS code never touches `Vec<Node>`. It only calls `builder.push_batch(...)`.**

That single constraint is the difference between extracting `FsScanner` in an
afternoon and rewriting the core. It costs nothing to obey while NTFS is the
only scanner. The trait itself is not written until M7 - see
[architecture.md](./architecture.md) sec 13.

The tradeoff being accepted: the seam goes unvalidated by a second
implementation until M7, so any mistake in its shape surfaces late. The
mitigation is the rule above plus the `RecordingSink` test harness in M0, which
exercises the sink boundary from day one even with only one producer.

---

## M0 - Foundation

**Goal:** the shared, filesystem-agnostic core exists and is tested, before any
NTFS code is written against it.

**Lands**

- `model/index.rs` - `Index`, `Node`, `NodeId`, arena, CSR children ([architecture.md](./architecture.md) sec 7).
- `model/builder.rs` - `IndexBuilder`: the `EntrySink`. Intern -> resolve -> link -> rollup, stages 2-5 of the build pipeline (sec 8).
- `model/entry.rs` - `RawEntry`, `EntryFlags`, normalized `Times`.
- `model/caps.rs` - `VolumeCaps` ([features.md](./features.md) sec 1.3, sec 1.4).
- `parser/block.rs` - `BlockSource`: volume handle, physical drive + partition offset, image file. Sector-aligned buffer helper.
- Volume enumeration and probing ([features.md](./features.md) sec 1.1): drive letters, `FindFirstVolume` for letterless volumes and mount points, filesystem-type sniff.
- Elevation detection and one-click relaunch ([features.md](./features.md) sec 1.1).
- `error.rs` variants: `Volume`, `Filesystem`, `Unsupported` (STANDARDS sec 4.1).
- `RecordingSink` + `ValidatingSink` test doubles.
- **Platform gating.** `emfit-core` must compile on Linux and macOS with the Windows-only modules `#[cfg]`-ed out, or CI cannot be green on three platforms (STANDARDS sec 5.2). Design this now; retrofitting `cfg` boundaries is miserable.

**Exit criteria**

- Feeding synthetic entries through `IndexBuilder` produces a correct tree: parents resolved regardless of arrival order, CSR children correct, rollup sizes and counts correct.
- Rollup verified against a hand-computed fixture tree including a deep chain, a wide directory, and an orphan.
- `cargo check` clean on Windows, Linux, and macOS.
- `cargo clippy` clean with workspace lints; `cargo fmt` clean.

**Risk:** designing `Node` for a filesystem that doesn't exist yet. Mitigation -
keep it to the fields in sec 7 and put anything filesystem-shaped in a side table.

---

## M1 - NTFS engine

**Goal:** scan a real volume, correctly and fast, verifiable without a UI.

**Lands**

- MFT location and extent mapping, both modes ([features.md](./features.md) sec 1.2 steps 1-2): `FSCTL_GET_RETRIEVAL_POINTERS` in volume mode, record 0 `$DATA` runs in physical mode.
- Bulk sequential read with **extent clamping**, **sector-aligned buffers**, and **overlapped double-buffering** (sec 1.2 step 3). The clamping bug is v1's - batches that span a fragment boundary silently read unrelated data.
- Record validation, fixup with `bytes_per_sector` stride, in-use check *before* parsing (sec 1.2 step 4).
- Attribute walk: `$STANDARD_INFORMATION`, all `$FILE_NAME` (hard links, DOS names dropped), `$DATA` resident and non-resident, `$ATTRIBUTE_LIST` (sec 1.2 step 5).
- Extension-record resolution from the sequential stream, single read per record (sec 1.2 step 6).
- Parse in place, no per-record allocation, rayon across the buffer, no logging in the hot path (sec 1.2 optimizations).
- Reparse points and junctions flagged and not followed; ADS side table; free space and system-reserve rows ([features.md](./features.md) sec 1.3).
- Multi-volume scan in parallel; per-volume failure isolation ([features.md](./features.md) sec 7).
- `Progress` + `CancellationToken` threaded through, with `ControlFlow` honored on every batch.
- BenchLog JSONL per STANDARDS sec 5.5 ([features.md](./features.md) sec 1.7).
- **Verification CLI** - a thin third binary over the core, enough to test with: `volumes`, `scan`, `stats`, `read-mft`, `tree-size`. The rest of [features.md](./features.md) sec 10 waits for M6.

**Exit criteria**

- Full C: scan completes in **a few seconds** ([features.md](./features.md) sec 1.7), recorded in BenchLog.
- File and directory counts and total size **match WizTree** on the same volume, within a documented tolerance for hard links and free-space accounting.
- Correct on a **fragmented MFT** - verified against a volume with a multi-extent `$MFT`, which is what the clamping fix is for.
- Correct on a **4Kn drive** or a synthetic fixture with 4096-byte sectors.
- Cancel stops a scan within ~100 ms.
- Peak RSS documented and within budget for a 5M-file volume.
- Parser tests against fixture MFT records in `emfit-core/tests/fixtures/` (STANDARDS sec 5.1).

**Risk:** this is the milestone that decides whether the rewrite was worth doing.
Measure against WizTree continuously, not at the end.

---

## M2 - Shell, list, search

**Goal:** first usable application. Scan, see everything, find anything.

**Lands**

- Tauri command surface and the **row-window IPC contract** ([features.md](./features.md) sec 9): the index never crosses the boundary; the frontend requests `rows(offset, count)` against the current query and sort.
- Typed `ipc.ts` mirroring the Rust signatures; `npm run check` clean (STANDARDS sec 5.1).
- Scan progress and completion as Tauri events, debounced ([features.md](./features.md) sec 7).
- Virtualized file list: Name, Path, Size, Extension, Date Modified, Type, Allocated ([features.md](./features.md) sec 3).
- Sorting off the UI thread, stable and cached; multi-selection with ctrl/shift.
- File type icons and per-type coloring.
- Search ([features.md](./features.md) sec 2): substring, wildcards, multi-pattern, path scoping, regex, size / date / extension filters, `Filters.csv` presets, inline `ext:` `size:` `dm:` `folder:` operators.
- **Case folding from `caps.case_sensitive`**, allocation-free against the arena, `$Upcase` on NTFS.
- Debounced, interruptible query; result count and elapsed time.
- Status bar: object count, selected count, selection total, volume total.
- Scan UX: drive selection, cancel button actually wired, per-volume failure shown inline, hidden/system toggles.

**Exit criteria**

- Typing in the search box updates results **within one frame** on a 5M-file index.
- Scrolling the full list is smooth; no full-set transfer to the webview at any point.
- Sorting 5M rows does not block the UI.
- Cancel visibly works.
- `npm run check` and `cargo clippy` clean; bundle builds in CI on three platforms.

**Risk:** IPC chattiness. Budget the row-window payload early - pre-formatted
display strings plus ids, not full nodes.

---

## M3 - Space analysis

**Goal:** the WizTree half. This is the milestone that makes EmFit worth using
over Everything.

**Lands**

- **Folder tree pane** ([features.md](./features.md) sec 4.1) - the largest single parity gap. Expandable, size-sorted, inline proportional bars, lazy child materialization from CSR ranges, counts and percentages per row, keyboard navigation, in-place filter.
- **Treemap** ([features.md](./features.md) sec 4.2): squarified hierarchical layout **computed in Rust** and sent as a flat rectangle list for the current canvas and drill level (sec 9). Drill down, breadcrumb, hover tooltip and highlight, color by type or by folder with a legend, adjustable depth, free-space block, zoom-to-selection.
- **File type breakdown** ([features.md](./features.md) sec 4.3): aggregate by extension, click to filter the list and treemap, multi-select.
- **Top-N largest** files and folders as a view ([features.md](./features.md) sec 4.4).
- Selection synchronized bidirectionally across tree, treemap, and list.
- Real vs allocated size switchable everywhere; honest labeling where `sizes_are_exact` is false.

**Exit criteria**

- Treemap layout under **100 ms** at any drill level ([features.md](./features.md) sec 1.7).
- Expanding a directory with 100k children does not stall.
- Folder tree totals agree with the treemap and with WizTree on the same volume.
- Free space plus accounted files equals volume capacity, within a documented tolerance.

**Risk:** treemap layout in Rust with rendering in canvas is the right split but
a new one for this codebase. Prototype the payload shape before building the
full view.

---

## M4 - Operations & export

**Goal:** act on what you found, and get it out.

**Rescoped 2026-07-30 (user direction):** EmFit ships **no mutating file
operations of its own**. Right-click hands the node to the **real Windows
shell context menu** (`IContextMenu`), exactly as WizTree does; deleting,
renaming, and moving are the shell's entries and the shell's business. The
only EmFit-added item is **Copy path**. Because nothing mutates through the
app, the index cannot observe changes from its own actions - index updating
therefore moves to M5, where the USN journal is the observer, and shrinks
to deletion-marking only (see M5).

**Lands**

- Right-click in **every view** - file list, folder tree, treemap - opens
  the native shell context menu for that file or folder, with one appended
  EmFit item: **Copy path**. No other custom entries, none mutating.
- Export ([features.md](./features.md) sec 6): CSV and JSON via `serde_json` with correct escaping (v1's hand-rolled writers are malformed for names containing quotes), streaming rather than in-memory, **exports what is currently shown** by default, folder tree export, treemap PNG.
- Row detail pane: full path, all timestamps, attributes, hard links and their paths, alternate data streams.

**Exit criteria**

- Right-click on a row/rect in each of the three views opens the shell menu
  for the correct path, including shell-extension submenus ("Open with",
  "Send to"), and its verbs execute.
- **Copy path** puts the full absolute path on the clipboard; the menu
  contains no other EmFit-added or mutating-custom entries.
- Round-trip test: export CSV and JSON containing names with quotes, commas, newlines, emoji, and non-BMP characters; re-import and compare.

---

## M5 - Live updates & persistence

**Goal:** the index stops going stale, and scans survive a restart.

**Lands**

- USN journal watcher ([features.md](./features.md) sec 1.5), **rescoped to
  deletion-marking only** (user direction 2026-07-30): a node observed
  deleted is *marked* deleted - red border in the treemap, flagged in the
  tree and list - and stays marked until the next rescan. Nothing is
  removed from the index, no sizes re-roll. Creations, renames, size
  changes, and even the reappearance of a marked path change **nothing**;
  only deletions register. Gated on `caps.live_updates`.
- Journal-gap recovery - wrapped journal or changed id falls back to a full rescan rather than diverging silently.
- Scan save/load ([features.md](./features.md) sec 1.6) - serialize the flat index and arena. **This is the `1.0.0` gate**, because the format becomes a compatibility promise once anyone has a saved scan.
- Per-volume scan cache reloaded at startup, with volume serial and timestamp so staleness is visible.
- Exclusion list applied at index time.

**Exit criteria**

- Deleting a file in Explorer marks it deleted in every view within a second (red border in the treemap); creating, renaming, or restoring files changes nothing until a rescan.
- A scan saved and reloaded is byte-identical in its query results to the original.
- Format carries a `schema_version` (STANDARDS sec 4.5) and endianness is decided and documented.
- Killing the app mid-scan leaves no corrupt cache.

**Risk:** incremental rollup is where subtle size drift creeps in. Add an
assertion mode that re-rolls the whole index and compares.

---

## M6 - Polish & release readiness

**Goal:** `1.0.0`.

**Lands**

- Settings panel ([features.md](./features.md) sec 8): default drives, size units, real-vs-allocated default, treemap depth and color mode, hidden/system, elevation preference, exclusions.
- Light theme and follow-system, on the existing design tokens.
- Window and layout state persistence; column show/hide, reorder, resize.
- Full keyboard operability and the shortcuts dialog; **resolve the F5-vs-F9 rescan conflict** between v1 and the README.
- Single-instance with re-focus; portable mode (config beside the executable) for the forensic use case.
- Search history and saved searches.
- Full CLI surface ([features.md](./features.md) sec 10) with stable exit codes; diagnostics behind `diag` - **strip the hardcoded record numbers** v1 baked into its `debug` command.
- About panel surfacing bundled license texts (STANDARDS sec 5.6).
- README brought current: screenshot, feature list, shortcut table (STANDARDS sec 5.3).

**Exit criteria**

- Every feature reachable by keyboard alone.
- Fresh install on a clean Windows 11 machine works without a dev toolchain.
- Portable mode leaves nothing in `%APPDATA%`.
- CI green on three platforms including a full bundle build.

---

## M7 - Additional filesystems

**Goal:** cash in the architecture. Everything above stays untouched.

**Lands, in order**

1. **Extract `FsScanner` and `ScannerRegistry`** ([architecture.md](./architecture.md) sec 5, sec 10). NTFS becomes the first registered implementation. If this extraction requires changing `Index`, `Node`, search, treemap, or export, the seam was wrong - fix the seam, not the model.
2. **Directory-walk scanner** - `FindFirstFileEx` with `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`. Unblocks FAT32, exFAT, ReFS, network shares, and mounted images ([features.md](./features.md) sec 1.1). Its parent->child linkage is the opposite of NTFS, which is exactly what makes it the seam's real test.
3. **exFAT / FAT32 native** - small spec, no stable file ids (`has_stable_ids: false`).
4. **ext4** - inode preload then directory descent; extent-tree decoding is the bulk of it. Almost certainly arriving as an image, so MBR/GPT partition parsing lands here too.
5. Beyond that: Btrfs, APFS, ZFS - each per the checklist in [architecture.md](./architecture.md) sec 11, and ZFS additionally needs the `sizes_are_exact: false` story answered in the UI before it is worth shipping.

**Exit criteria per scanner**

- Fixture image in `emfit-core/tests/fixtures/`, integration test asserting a known tree (STANDARDS sec 5.1).
- `VolumeCaps` declared honestly; UI degrades correctly (columns hide, live updates disabled, case sensitivity follows the volume).
- Nothing above `model/` was modified to accommodate it.

---

## Cross-cutting, every milestone

- **Tests:** integration tests in `emfit-core/tests/` against the real public API; every bug fix gets a regression test; every new format gets a fixture (STANDARDS sec 5.1).
- **CI green on three platforms** - fmt, clippy (deny warnings), `cargo test`, `svelte-check`, bundle build (STANDARDS sec 5.2).
- **README updated in the same PR** as any change touching its listed sections (STANDARDS sec 5.3).
- **BenchLog** entries for every operation whose speed a user notices (STANDARDS sec 5.5).
- **`analysis_todo.md`** absorbs ideas that surface mid-milestone; promote to issues, delete what goes stale (STANDARDS sec 5.4).

## Deferred, with a reason

| Item | Milestone | Why not sooner |
|---|---|---|
| `FsScanner` trait | M7 | Premature with one implementation; the push discipline preserves the option |
| Scan file format | M5 | Becomes a compatibility promise the moment it ships |
| Full CLI surface | M6 | A minimal verification CLI in M1 covers the testing need |
| USN live updates | M5 | Deletion-marking only; the journal is the sole observer since M4 ships no mutating operations |
| Out-of-process scanners | - | Attractive for sandboxing untrusted images; not needed for NTFS ([architecture.md](./architecture.md) sec 13) |
