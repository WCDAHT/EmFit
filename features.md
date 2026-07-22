# EmFit — Feature Specification

What EmFit does, and what it must do to stand next to WizTree and Everything.
This is a capability inventory, not a plan: no ordering, no milestones, no
estimates. A roadmap gets derived from this document separately.

**Product definition.** A Windows desktop tool that reads NTFS volume metadata
directly instead of walking directories, giving two things at once:

1. **Search** — type a fragment, get every matching file on every volume, instantly (Everything's job).
2. **Space analysis** — see what is actually consuming the disk, as a tree and as a treemap (WizTree's job).

The rewrite target is Rust (`emfit-core`) + Tauri 2 shell + Svelte 5 frontend, per
[STANDARDS.md](./STANDARDS.md). All logic lives in `emfit-core` and is testable headless.

**NTFS is the first filesystem, not the only one.** The engine is structured so
additional filesystems (the directory-walk fallback first, then ext4, exFAT, and
whatever follows) plug in behind one narrow seam without touching the index,
search, treemap, or export. [architecture.md](./architecture.md) defines that
seam and the shared data model; where a feature below is filesystem-specific it
says so, and where it depends on a capability the volume may not have it names
the capability.

## Status tags

| Tag | Meaning |
|-----|---------|
| `[v1]` | Working in the egui/TUI version. Port it. |
| `[v1-partial]` | Exists in v1 but incomplete, wrong, or stubbed. Re-do it. |
| `[new]` | Not in v1. Parity gap against WizTree/Everything, or required by the new architecture. |

---

# 1. Core engine

## 1.1 Volume discovery and access

- `[v1]` **Enumerate NTFS volumes.** Probe drive letters `A:`–`Z:`, keep the ones that answer `FSCTL_GET_NTFS_VOLUME_DATA`. Report per-volume total/free bytes, cluster size, label, serial.
- `[v1]` **Dual access modes.** Physical-drive mode by default (`\\.\PhysicalDriveN` + partition offset from `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`), volume-handle mode (`\\.\C:`) as fallback. Physical mode bypasses the NTFS driver entirely and survives cases where the filesystem refuses IOCTLs.
- `[v1]` **Boot-sector parsing** for physical mode, since `FSCTL_GET_NTFS_VOLUME_DATA` isn't available there: bytes/sector, sectors/cluster, MFT start LCN, bytes per MFT record, volume serial.
- `[new]` **Elevation.** Raw volume reads require Administrator. Detect the token at startup; if not elevated, offer a one-click relaunch via `runas` rather than failing with an access-denied error. Remember the choice.
- `[new]` **Non-NTFS fallback.** FAT32, exFAT, ReFS, network shares, and mounted images have no MFT. Fall back to a parallel directory walk (`FindFirstFileEx` with `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`) producing the same index. WizTree does this; without it the app simply cannot open half the drives a user will try. This is also the **second scanner implementation**, and the one that proves the seam in [architecture.md](./architecture.md) §5 is in the right place — its linkage runs parent→child, the opposite of NTFS.
- `[new]` **Mount points and volumes without a letter.** Enumerate via `FindFirstVolume`/`GetVolumePathNamesForVolumeName` so mounted folders and letterless volumes are scannable.
- `[new]` **Scan a single folder, not just a whole volume.** WizTree's "select folder" mode. Implementation: scan the volume's MFT as usual, then filter the index to the subtree rooted at that path — still far faster than a directory walk, and it keeps one code path.
- `[new]` **Drag-and-drop a folder or drive onto the window** to scan it.

## 1.2 MFT read and parse flow

The pipeline, in order. Stages 1–5 are v1 logic that works and should be carried
over conceptually; the optimizations noted are *not* in v1 and are the point of
the rewrite.

1. **Locate the MFT.** `FSCTL_GET_NTFS_VOLUME_DATA` for `MftStartLcn` and `MftValidDataLength` (volume mode), or the boot sector (physical mode).
2. **Map MFT extents.** `[v1]` The MFT is fragmented on any real volume and must not be assumed contiguous. Volume mode: `FSCTL_GET_RETRIEVAL_POINTERS` on `C:\$MFT`. Physical mode: parse record 0's `$DATA` data runs directly — this also recovers `MftValidDataLength`, which physical mode otherwise lacks.
3. **Bulk sequential read.** `[v1-partial]` Read multi-megabyte chunks along the extent map, not record-at-a-time.
   - `[new]` **Clamp each read to the current extent.** v1 computes the offset of the first record in a batch and then reads `count × 1024` bytes straight through, so any batch spanning a fragment boundary silently ingests unrelated on-disk data. Reads must stop at the extent edge.
   - `[new]` **Sector-aligned buffers.** The handle is opened `FILE_FLAG_NO_BUFFERING`, which requires the *buffer address* — not just offset and length — to be sector-aligned. v1's `vec![0u8; 1024]` is aligned only by luck.
   - `[new]` **Overlapped, double-buffered I/O.** v1 is a strictly serial read→parse→read loop with queue depth 1; the disk idles during every parse. A reader thread issuing overlapped reads into a channel keeps both busy.
4. **Validate and fix up each record.** `[v1]` Check the `FILE` signature; apply the Update Sequence Array, restoring the last two bytes of every sector from the fixup array. Skipping this corrupts filenames.
   - `[new]` Drive the fixup stride from `bytes_per_sector`, not a hardcoded 512 — native 4Kn drives break otherwise.
   - `[new]` Test the in-use flag *before* applying fixups and parsing attributes. A large fraction of MFT records are deleted; v1 pays full parse cost for all of them.
5. **Walk attributes.** `[v1]` Iterate by attribute length from `first_attribute_offset`:
   - `0x10 $STANDARD_INFORMATION` — timestamps (created/modified/accessed) and file attribute flags (hidden, system, compressed, sparse, reparse point, directory).
   - `0x30 $FILE_NAME` — parent record reference, namespace, name. Collect **all** of them: DOS-only (8.3) names get dropped, and each remaining one with a distinct parent is a hard link. Fall back to `$FILE_NAME` timestamps when `$STANDARD_INFORMATION` is missing.
   - `0x80 $DATA` — real size and allocated size; resident vs non-resident via the form code. Named `$DATA` streams are alternate data streams.
   - `0x20 $ATTRIBUTE_LIST` — records whose attributes spilled into extension records. Note which extension records hold `$FILE_NAME` and which holds the primary `$DATA` (starting VCN 0, unnamed).
6. **Resolve extension records.** `[v1-partial]` Records with an attribute list need a second look to recover their name, hard links, and size. v1 reads each extension record twice, with a separate 1 KB random unbuffered read each time. Buffer the pending set and satisfy it from the sequential stream instead of seeking — extension records almost always sit near their base record.
7. **Build the index** (§1.3) and **roll up sizes** (§1.4).

### Parse-stage optimizations to build in from the start

These are the specific reasons v1 is slow. Each is cheap to do correctly the
first time and expensive to retrofit.

- **Parse in place.** v1 copies every 1024-byte record out of the read buffer into its own `Vec<u8>` — roughly 5 GB of allocation churn for a full C: scan. Parse over slices of the read buffer.
- **No per-record heap allocations.** v1's per-file struct carries a `String`, a `Vec<DataRun>`, a `HashMap` of alternate streams, and two more `Vec`s. Of those, the data runs and the ADS map are decoded for every file and read by nothing. Names go into a shared UTF-8 arena; everything else is a fixed-size record.
- **Parallel parse.** Record parsing is embarrassingly parallel. Fan chunks out with rayon into per-thread output buffers and merge.
- **Parallel across volumes.** Scanning C: and D: at once is nearly free when they are separate physical disks. v1 does them in sequence.
- **No logging in the hot path.** v1 formats a log line — including cloning every hard-link name into a fresh `Vec` — for all ~5M records, then throws it away because the filter is off, taking a global mutex each time. Any per-record diagnostic must be behind an atomic flag checked before the arguments are built.

## 1.3 Index data model

Shared by every filesystem; see [architecture.md](./architecture.md) §7 for the
struct definitions and the three rules that keep it portable.

- `[new]` **Flat, index-addressed store.** Nodes in a `Vec`, addressed by a dense `NodeId` the builder assigns. Fixed-size node: parent id, name offset+length into the arena, size, allocated size, normalized timestamps, flags, child range. v1 uses three `DashMap`s per file plus a per-node `String` and a per-node children `Vec`.
- `[new]` **`NodeId` is ours, not the filesystem's.** v1 indexes the node array *by MFT record number* and hardcodes "root is record 5", which welds NTFS into the model's spine. The filesystem-native id (MFT record, inode) lives in a side table, present only when the filesystem has stable ids at all — FAT32 has none.
- `[new]` **String arena.** All names in one contiguous UTF-8 buffer; nodes hold `(u32 offset, u16 len)`. v1 additionally stores a *lowercased copy of every name on the volume* as a hash-map key, purely for deduplication.
- `[new]` **CSR child lists.** Count children per parent, prefix-sum, fill — one O(n) pass. v1 pushes into a per-parent `Vec` guarded by a linear `contains()` scan, which is O(k²) per directory, and then repeats the entire operation a second time in `build()`. On directories with tens of thousands of entries this dominates the whole scan.
- `[new]` **Per-filesystem extras live in side tables keyed by `NodeId`,** never as fields on the node. Nodes stay small because node size is search speed — at 5M files every extra 8 bytes is 40 MB and a cache miss on every search, sort, and rollup pass.
- `[new]` **Normalized timestamps.** One internal representation (nanoseconds since the Unix epoch), converted inside the scanner. NTFS ticks from 1601, ext4 counts from 1970, FAT32 has two-second resolution; v1 stores raw `FILETIME` through every layer.
- `[new]` **Root and path assembly come from the volume,** not a constant. The scanner declares its root node; separator and root label come from `VolumeCaps`. v1 builds paths with a hardcoded `format!("{}:\\{}", drive_letter, …)`.
- `[v1]` **Hard links.** On NTFS: one MFT record, several `$FILE_NAME` attributes with different parents. Every link is a distinct row in the file list with its own path, but they share one underlying record, and size must be attributed once — not once per link — in the rollup, or WinSxS inflates the totals wildly. Gated on `caps.has_hard_links`.
- `[v1]` **Orphans** (parent missing from the index) are counted and surfaced in stats rather than dropped silently.
- `[new]` **Alternate data streams.** Counted toward the owning file's size (WizTree's behavior); optionally listable per file. An NTFS-only side table — ext4's nearest equivalent is extended attributes, which are a different thing and get their own. Cheap to keep once `$DATA` is being walked anyway; the current version parses them and discards them.
- `[new]` **Reparse points, junctions, symlinks.** Flag them and do not follow — a junction must not have its target's bytes counted a second time.
- `[new]` **Free space and system reserve as first-class rows.** WizTree shows "Free space", `$MFT`, pagefile, hiberfil, and System Volume Information in the treemap so the rectangles add up to the physical disk. Without this the treemap silently under-accounts for a large chunk of the drive and looks wrong.

## 1.4 Size rollup

- `[v1-partial]` **Directory totals.** Every folder needs total size, allocated total, file count, folder count, and depth. v1 walks the tree building a `HashSet` of visited keys, a `HashMap` entry for all ~5M nodes, and a clone of every node's children vector.
- `[new]` Replace with a single pass in reverse topological order over the flat array (or accumulate into the parent slot as each file is inserted, which is O(1) per file once parents are array indices).
- `[new]` **Real size vs allocated size ("size on disk")** tracked separately everywhere and switchable in the UI. This is the whole point of a space analyzer on a volume with compression, sparse files, and small files padding to cluster size. Gated on `caps.has_allocated_size`; where a filesystem can't report it, allocated equals real and the column hides.
- `[new]` **Percent of parent** and **percent of volume**, precomputed for display.
- `[new]` **Honest totals.** Where `caps.sizes_are_exact` is false — copy-on-write filesystems whose snapshots and clones share blocks, so two files can each truthfully report 1 GB while together occupying 1 GB — the UI says so rather than presenting a confident wrong number. Not an issue for NTFS; it becomes one the day a ZFS or APFS image is opened.

## 1.5 Live updates (USN journal) — NTFS only

Gated on `caps.live_updates`. The USN journal has no equivalent on ext4 or FAT,
so every feature here degrades to "rescan to refresh" on other filesystems, and
the UI must not imply otherwise.

- `[v1-partial]` **USN journal enumeration** (`FSCTL_ENUM_USN_DATA`) as an alternative fast path. It yields names and parents but no sizes, so v1 uses it only in combination with an MFT pass. Keep it as an option; it is not the primary path.
- `[v1-partial]` **Change monitoring** (`FSCTL_READ_USN_JOURNAL`). v1 has the plumbing and a `ChangeMonitor` type, but create/delete/rename handling is stubbed — the CLI command prints "real-time monitoring would run here".
- `[new]` **Finish it.** A background watcher that applies creates, deletes, renames, and size changes to the live index, so a scan does not go stale while the window is open. This is the single biggest UX difference between Everything and every other search tool. Requires: incremental insert/remove against the flat index, incremental rollup fix-up along the parent chain, and a debounced UI refresh.
- `[new]` **Journal-gap recovery.** If the journal wrapped or the ID changed since last read, fall back to a full rescan rather than silently diverging.

## 1.6 Persistence

- `[new]` **Save and load a scan.** WizTree writes a scan file you can reopen without rescanning (and share). Serialize the flat index + arena; it is nearly a straight memory dump. Also enables opening a scan taken on another machine — directly useful for forensic work.
- `[new]` **Cache the last scan per volume** and reload it at startup so the window is never empty, with the volume serial + timestamp recorded so a stale cache is obvious and a rescan is one key away.

## 1.7 Performance targets and instrumentation

- `[new]` **Targets.** Full C: (≈1M files) in a few seconds; search results updating within a frame of each keystroke; treemap layout under 100 ms at any drill level. WizTree is the benchmark to beat, and it is beatable — the v1 engine is not slow because of NTFS, it is slow because of allocation churn and quadratic child linking.
- `[new]` **Scan targets are per-scanner.** "Sequential read at disk bandwidth" is a property of NTFS's single-table layout, not a promise the architecture can make for filesystems that must traverse directories to enumerate. Search, sort, treemap, and rollup targets are shared, because those run against the index and don't care where it came from.
- `[new]` **BenchLog** per STANDARDS §5.5: append-only JSONL in `logs/`, one event per scan with `duration_ms`, record count, file count, volume, mode (physical/volume/fallback). Local only.
- `[new]` **Phase timings** surfaced in the UI (read / parse / index / rollup) so regressions are visible without a profiler.

---

# 2. Search

- `[v1]` **Instant substring match** on filename, case-insensitive, over the whole index.
- `[v1]` **Wildcards.** `*.ext`, `prefix*`, `*text*`, plain substring.
- `[v1]` **Multi-pattern.** Semicolon-separated terms, OR'd: `.pdf;.docx`.
- `[v1]` **Path scoping.** A backtick-quoted path prefix restricts results to a subtree.
- `[v1]` **Regex** as a separate filter field.
- `[v1]` **Size filter.** Greater than / less than / between, with human units (`10MB`, `1GB`, `500KB`).
- `[v1]` **Date-modified filter.** After / before / between, `YYYY-MM-DD`.
- `[v1]` **Extension filter.** Semicolon-separated list.
- `[v1]` **Preset filters** loaded from `Filters.csv` — Everything's format, currently: Everything, Audio, Compressed, Document, Executable, Folder, Picture, Video. Keep the file format so users can drop in their own.
- `[v1]` **Clear all filters** action, and a visual indicator when any filter is active.
- `[new]` **Search the full path, not just the name**, as a toggle. Everything defaults to name; path search is what people reach for next.
- `[new]` **Case-sensitive, whole-word, and diacritic-insensitive toggles.** The `Filters.csv` format already carries these columns; v1 parses and ignores them.
- `[new]` **Case sensitivity defaults to the volume, not to a global setting.** NTFS is case-insensitive; ext4 is case-*sensitive* by default. The default comes from `caps.case_sensitive` and the user can override it per search. This is the one capability that reaches up into otherwise fully shared code, so it has to be threaded through the matcher from the start rather than assumed.
- `[new]` **Allocation-free case folding.** v1 calls `to_lowercase()` on every name for every keystroke — millions of `String` allocations per query. Fold in place against the arena bytes instead. On NTFS the correct table is `$Upcase`, the 128 KB uppercase map in MFT record 10 that the filesystem itself compares with; load it once per volume. Other filesystems supply their own rule (or none), so folding is a property the scanner hands over, not a hardcoded function.
- `[new]` **Incremental / debounced search** that stays responsive while typing on a multi-million-file index, with an interruptible query so an in-flight search is abandoned when the next character arrives.
- `[new]` **Result count and elapsed time** displayed with the results.
- `[new]` **Search history** (recent queries, arrow-key recall) and **saved searches**.
- `[new]` **Filter to files only / folders only.**
- `[new]` **`ext:`, `size:`, `dm:`, `folder:` inline operators** in the query string, the way Everything and the `Filters.csv` presets already express them — the preset file uses `ext:` syntax that v1 has to special-case because the query language does not actually support it.

---

# 3. File list

- `[v1]` **Virtualized table** over the filtered result set. Non-negotiable at these row counts.
- `[v1]` **Columns:** Name, Path, Size, Extension, Date Modified, Type.
- `[new]` **Additional columns:** Allocated size, Attributes, Date Created, Date Accessed, Items/Files/Folders (for directories), % of parent, native record id. Columns backed by a capability the volume lacks (allocated size, native id, hard-link count) hide rather than showing zeros.
- `[new]` **Column show/hide, reorder, and resize**, persisted.
- `[v1]` **Sort by any column, ascending/descending**, click header to toggle, indicator arrow. Sorting runs off the UI thread with a progress state (v1 already does this; at these sizes it matters).
- `[new]` **Stable, cached sort** so re-sorting a filtered view doesn't re-sort the world.
- `[v1]` **Multi-selection:** click, ctrl+click, shift+click ranges, select-all, keyboard range extension.
- `[v1]` **File type icons and per-type coloring** by extension (executables, archives, images, source, documents, media…).
- `[v1]` **Human-readable sizes.**
- `[new]` **Size unit override** (auto / KB / MB / GB) and **thousands separators**, per WizTree.
- `[v1]` **Status bar:** object count, selected count, total size.
- `[new]` **Selection total** — sum of selected rows' sizes, shown live. Users select a batch specifically to answer "how much will this free up".
- `[v1-partial]` **On-demand metadata refresh.** v1 re-opens files by `OpenFileById` in batches to fix up sizes and timestamps the MFT parse got wrong. Treat this as a bug to fix in the parser rather than a feature to port; keep it only as an explicit "verify against filesystem" action.
- `[new]` **Row detail / preview pane** showing full path, all timestamps, attributes, hard-link count and their paths, alternate data streams.

---

# 4. Space analysis views

## 4.1 Folder tree (WizTree's main pane) — `[new]`

The most important missing view. v1 has a flat file list and a treemap but no
hierarchical folder browser, which is the primary way people actually use
WizTree.

- Expandable tree rooted at the volume, children sorted by size descending by default.
- Per row: name, size, allocated, % of parent as an **inline proportional bar**, item/file/folder counts, modified date, attributes.
- Lazy child materialization from the CSR ranges — expanding a 100k-child folder must not stall.
- Keyboard navigation, expand-all/collapse-all, expand-to-depth-N.
- Selection synchronized bidirectionally with the treemap.
- Search box that filters the tree in place.

## 4.2 Treemap

- `[v1]` **Squarified hierarchical treemap** with nested containers, depth-limited (v1 caps at 8), sizes derived from the rollup.
- `[v1]` **Drill down** (double-click) and **go up** (Back/Backspace), with a **breadcrumb** trail.
- `[v1]` **Selection highlight** and an info bar showing the selected item's name and size.
- `[v1]` **Depth-shaded container backgrounds and borders**; leaf color by file type.
- `[v1]` **Labels** — folder name + size on containers, name + size on leaves, drawn only where the rectangle is large enough.
- `[v1]` **Multi-volume:** all scanned drives laid out in one map.
- `[v1]` **Rebuild on canvas resize.**
- `[new]` **Hover tooltip** with full path and size, and **hover highlight** — v1 requires clicking to learn what a rectangle is.
- `[new]` **Color modes:** by file type (with legend) *or* by folder, switchable. WizTree ships both.
- `[new]` **Right-click context menu** on a rectangle (open, explorer, delete, copy path), matching the file list.
- `[new]` **Show/hide files vs folders**, and an adjustable depth limit, exposed in the UI rather than a constant.
- `[new]` **Free space rendered as a block** so the map accounts for the whole volume.
- `[new]` **Selection synchronized with the folder tree and file list.**
- `[new]` **Zoom-to-selection animation** — a drill-down that visually connects the old and new frame is what makes a treemap navigable rather than disorienting.

## 4.3 File type breakdown — `[new]`

WizTree's "File Types" tab; entirely absent from v1. Aggregate the index by
extension: count, total size, allocated, % of volume, sorted by size. Clicking a
type filters the treemap and file list to it. Checkbox multi-select for
comparing several types. This is how a user answers "what kind of thing is
eating my disk" in one glance.

## 4.4 Top-N largest — `[v1]`

- Largest files and largest folders. v1 has both (`largest_files`, `largest_directories`, CLI `largest`), but only as CLI output — surface them as a view with a configurable N (WizTree's "Top 1000 Largest Files" tab).

---

# 5. File operations and shell integration

- `[v1]` **Open** (shell default action).
- `[v1]` **Open containing folder in Explorer**, with the file selected (`explorer /select,`).
- `[v1]` **Windows Properties dialog** (`ShellExecuteEx` with the `properties` verb).
- `[v1]` **Copy path to clipboard**, multi-selection aware.
- `[v1]` **Rename**, with a confirmation dialog.
- `[v1]` **Delete**, with a confirmation dialog.
- `[new]` **Delete to Recycle Bin vs. permanent delete** as distinct actions (`SHFileOperation` / `IFileOperation`). v1 only does one, which is dangerous in a tool whose entire purpose is finding big files to remove.
- `[new]` **Index updates after an operation** — deleted rows disappear, folder sizes re-roll up the parent chain, without a full rescan.
- `[v1]` **Native context menu** in the app (open / explorer / properties / copy path / rename / delete).
- `[new]` **Real Windows shell context menu** (`IContextMenu`) so Open With, Send To, and installed shell extensions are available.
- `[new]` **Copy / move to a chosen folder**, and **drag files out** of the app into Explorer.
- `[new]` **Copy as CSV / copy name only / copy full path list** variants.

---

# 6. Export and reporting

- `[v1-partial]` **CSV export.** v1 writes `Path,Name,Size,Allocated,IsDirectory,Modified` but does no quote escaping, so any filename containing `"` produces a malformed file.
- `[v1-partial]` **JSON export.** Hand-rolled string concatenation with a single `\\` replacement; not valid JSON for names containing quotes, control characters, or non-BMP text. Use `serde_json`.
- `[new]` **Export what is currently shown** (current filter, sort, and column set) as well as the whole index. Exporting a 5M-row file when the user wanted their 40 search results is the wrong default.
- `[new]` **Export the folder tree** with sizes and percentages, to CSV and to a plain-text indented tree.
- `[new]` **Treemap image export** (PNG) for reports.
- `[new]` **Streaming export** — write incrementally rather than building the document in memory.

---

# 7. Scanning UX

- `[v1]` **Background scan** with the UI live throughout.
- `[v1]` **Progress reporting** — phase name and record counts, driven off the estimated MFT record count.
- `[new]` **Accurate percentage and ETA**, plus files-per-second and MB/s.
- `[v1]` **Cancellation token** in the core. `[new]` Wire it to a visible Cancel button; v1 has the flag but the GUI never sets it.
- `[v1]` **Multi-drive selection** — which volumes to scan.
- `[new]` **Per-volume scan state** so one drive failing (access denied, not NTFS) doesn't abort the others, with the failure explained inline.
- `[v1]` **Rescan** (F9 in v1, F5 per the README's shortcut table — pick one).
- `[new]` **Auto-scan on launch** of the last-used selection, optional.
- `[v1]` **Include/exclude hidden and system files** — config exists in `ScanConfig`; expose it in the UI.
- `[new]` **Exclusion list** (paths, patterns) applied at index time.

---

# 8. Application shell

- `[v1]` **Dark theme.** `[new]` Light theme and follow-system, per the design tokens in `frontend/src/styles/theme.css`.
- `[new]` **Window state persistence** (size, position, maximized, split positions, column layout, last view).
- `[new]` **Settings panel** covering: default drives, size units, allocated-vs-real default, treemap depth and color mode, hidden/system inclusion, elevation preference, exclusion list.
- `[v1]` **Keyboard shortcuts dialog** and **About** dialog. `[new]` The About panel must surface the bundled license texts (Inter OFL, Bootstrap Icons MIT) per STANDARDS §5.6 — the scaffold already has this component.
- `[new]` **Full keyboard operability.** v1's shortcut set is the baseline: Ctrl+F filters, Ctrl+A select all, F9 rescan, T treemap, Enter open, Delete delete, F2 rename, Ctrl+C copy path, Ctrl+Q quit, F1–F6 column sorts (TUI). Add: Ctrl+F focuses the search box (the README's contract), Esc cancels/closes, F5 rescan, Alt+Left/Right for treemap navigation.
- `[new]` **Single-instance with re-focus**, so launching again surfaces the running window instead of starting a second multi-gigabyte index.
- `[new]` **Portable mode** — config next to the executable rather than in AppData, for running off a USB stick on a target machine. Directly relevant to the forensic use case.

---

# 9. Frontend / IPC architecture — `[new]`

Constraints the Tauri split imposes that the egui version never had. These are
implementation requirements, not features, but they shape every view above.

- **The index never crosses the IPC boundary.** It lives in `emfit-core`, owned by the Tauri shell's state. Serializing millions of rows to JSON would dwarf the scan itself.
- **Windowed row access.** The frontend asks for `rows(offset, count)` against the current query+sort and gets back only what the viewport shows, plus a total count. The virtual scroller drives the offset.
- **Sorting and filtering happen in Rust**, never in JavaScript, and never by shipping the full set to the webview to sort.
- **Progress and change events** flow shell → frontend via Tauri events (scan progress, index updated, USN change batch), debounced to a sane frame rate.
- **A row window is a compact payload** — pre-formatted display strings plus the ids needed for actions, not the full node.
- **The treemap layout is computed in Rust** and sent as a flat rectangle list for the current canvas size and drill level. Laying out hundreds of thousands of rectangles in JS is not viable; rendering them on a canvas from a precomputed list is.
- **Typed IPC contract** in `frontend/src/lib/ipc.ts` mirroring the Rust command signatures, with `npm run check` clean per STANDARDS §5.1.
- **Least-privilege capabilities** in `src-tauri/capabilities/` — the shell needs filesystem and shell-open permissions, and nothing wider.

---

# 10. Headless / CLI surface

v1 shipped a full CLI alongside the TUI and GUI. Worth keeping as a thin binary
over `emfit-core`, since it is trivially testable and useful for scripting;
WizTree has command-line export for the same reason.

- `[v1]` `volumes` — list NTFS volumes with total/free space.
- `[v1]` `scan --drive C [--usn] [--mft] [--no-physical] [--hidden] [--system] [--output text|json]` — scan and print statistics (files, dirs, total size, allocated, orphans, files/sec).
- `[v1]` `search --drive C <pattern> [--max N]` — search and print matching paths.
- `[v1]` `largest --drive C [--count N] [--dirs]` — top-N files or folders.
- `[v1]` `tree-size --drive C [--path P] [--depth N]` — indented size tree.
- `[v1]` `export --drive C --output FILE --format json|csv`.
- `[v1-partial]` `monitor --drive C` — real-time change monitoring; currently a stub that initializes and exits.
- `[new]` Exit codes and machine-readable output that a script can rely on.

## Diagnostics — `[v1]`, keep but quarantine

v1's `debug`, `read-mft`, and `usn-count` subcommands are genuinely useful for
NTFS work: dump a raw MFT record, trace a file's parent chain, report extent
maps and which extent a record falls in, count raw USN entries. Port them behind
a `diag` subcommand or a debug feature flag — but note that v1's `debug` command
has hardcoded record numbers from a past investigation baked into it, which
should not survive the move.

---

# 11. Explicit non-goals

Recording these so they don't get re-litigated during roadmapping.

- **Filesystems beyond NTFS and the directory-walk fallback, for now.** Not a permanent exclusion — the seam in [architecture.md](./architecture.md) exists precisely so ext4, exFAT, and others can be added — but nothing beyond those two is in scope until they are done, and ZFS-class filesystems additionally stress the size model (§1.4) in ways that need answering before they'd be worth shipping.
- **Cross-platform space analysis.** macOS/Linux builds exist only because CI builds all three per STANDARDS; the directory-walk fallback is what would run there, and no native metadata reader is planned for those platforms.
- Remote telemetry of any kind (STANDARDS §5.5 — these tools handle forensic evidence).
- File content indexing or content search. Names and metadata only.
- Duplicate-file detection by hash — a plausible future feature but a different engine (needs to read file contents, not just the MFT).
- Cloud storage / network share analysis beyond the directory-walk fallback.
