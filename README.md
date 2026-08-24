# EmFit

Fast local file search and disk-space analysis. EmFit reads NTFS volume metadata
directly instead of walking directories, so indexing a whole drive takes seconds:
type to find any file by name, or view what is actually consuming the disk.
A reimplementation of the ground covered by WizTree and Everything.

## Screenshot

<!-- Drop a screenshot or animated GIF of the current UI here. -->

## Install / run

Prerequisites: a [Rust toolchain](https://rustup.rs), [Node.js](https://nodejs.org) 20+,
and the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS
(on Linux, the WebKitGTK dev packages; on Windows/macOS the system webview is built in).

```powershell
npm install          # once, to pull frontend deps
npm run tauri dev     # run the app with hot-reload
```

`npm run tauri dev` starts the Vite dev server and the Rust shell together; edits
to Svelte/CSS hot-reload, edits to Rust trigger a recompile.

## Features

- **Fast NTFS scan** - reads the MFT directly (physical-drive mode with a
  volume-handle fallback), in parallel across drives, with live progress,
  a working Cancel, and per-drive failure isolation.
- **Instant search** across every scanned volume, with a full query language:
  `space` for AND, `|` for OR, `!` to exclude, `<>` to group, `""` for an exact
  phrase, wildcards (`*.pdf`, `draft*`, `n?tes`), backtick path scoping
  (`` `C:\Users` report ``), and modifiers and functions for everything else
  (`case:`, `path:`, `ww:`, `regex:`, `ext:iso size:>1gb dm:last7days`).
  **Search > Search syntax** lists the lot, and clicking an entry inserts it.
- **Advanced search** - a form over that language, reached from the filters
  menu: names, folder, dates, size, type, extension, attributes, regex,
  lengths, and what a folder holds. Every control says which syntax it writes,
  and anything it cannot express is carried through untouched.
- **Preset filters** you can edit: a two-column `Filters.csv` of a name and
  the search it runs. **Search > Edit filters...** writes a starter file from
  the built-in set (Audio, Video, Documents, ...) and opens it; reopening the
  filter menu picks up your changes without a restart. Everything's own
  `Filters.csv` drops straight in - its extra columns are ignored, because the
  search grammar already expresses them.
- **Case folding the volume's way** - name matching uses the volume's own
  NTFS `$Upcase` table, not a global rule.
- **Honest accounting**: hard links are listed under every name but their
  bytes count once; alternate data streams count toward their owner's size
  on disk; free space is a first-class row; reparse points are flagged and
  never followed.
- **Rescans are nearly free** - a finished scan is written to a compressed
  snapshot, and scanning that drive again reloads it and asks the NTFS change
  journal what changed since, re-reading only those records off the disk. The
  result is the same index a full sweep produces, in a fraction of the time.
  Nothing loads unasked: the cache is consulted when you scan a drive, never
  at startup. Switch it off under Settings > General.
- **Virtualized results list** - millions of rows scroll smoothly; only the
  visible window ever crosses from Rust to the UI. Sortable by any column,
  multi-select with Ctrl/Shift, live selection totals.
- **Lean chrome** - no in-app header; File/View/Help live in the native
  window menu. Scan sources are managed from a popup (+ to add a volume or
  a disk image, x to remove); all native disks are offered, only C: starts
  enabled.
- **Two tabs**: *List* (search, above) and *Tree view* - the WizTree half:
  - a folder tree with inline proportional bars, percent-of-parent, subtree
    counts, lazy expansion (100k-child folders open instantly), and keyboard
    navigation;
  - a **treemap** whose squarified layout is computed in Rust and painted on
    a canvas, WizTree-style: files as plain 1px-bordered boxes, folders with
    a reserved `name\ (999 GiB)` strip - drill down (double-click),
    breadcrumb, hover tooltips, adjustable depth, a height splitter, and
    free space drawn as a block so the map accounts for the whole volume.
    Colors follow **size buckets** (WizTree-style) or extension categories;
    both the mode and the buckets (ranges + colors) are editable in
    *Colors...* and persist in the appdata config;
  - a **file types** panel (what kind of thing is eating the disk); ticking
    types dims the map and filters the list;
  - a logical-vs-on-disk size switch. Selection is shared: click a rectangle
    and the tree reveals it, and vice versa. Treemap geometry is always
    allocated bytes, so hard-linked and sparse data is never drawn larger
    than it is.

### Coming next

- **Native Explorer right-click menu** on rows (`IContextMenu`, like
  WizTree), along with open/delete/copy-path operations and export -
  milestone **M4** (`0.4.0`).

## Keyboard shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+F` | Focus search |
| `F5`     | Rescan volume |
| `F1`     | Help |
| `Esc`    | Cancel / close |

## Architecture

A Cargo workspace of two Rust crates plus a Svelte frontend. `emfit-core` holds
all logic and depends on no UI or Tauri code. `src-tauri` is a thin Tauri shell that
wires `#[tauri::command]`s to the core. `frontend/` is the Svelte 5 + TypeScript app,
which talks to the shell only through typed IPC. `Cargo.toml` and `package.json` stay
at the root (the CLIs resolve their code dirs from there); everything else is grouped
under the three code peers.

```
EmFit/
+-- Cargo.toml            # Rust workspace
+-- package.json          # frontend deps + scripts (dev -> vite frontend)
+-- emfit-core/           # logic; testable headless
|   +-- src/{model,parser,service,error.rs,lib.rs}
|   +-- src/bin/emfit-cli.rs  # verification CLI: volumes, scan, stats, read-mft, tree-size, cache
|   +-- tests/fixtures/   # fixture MFT records for parser tests
+-- src-tauri/            # Tauri shell (Rust)
|   +-- src/{main.rs,lib.rs,commands.rs,error.rs}
|   +-- capabilities/     # least-privilege permission grants
|   +-- icons/            # generated by `tauri icon`
|   +-- tauri.conf.json
+-- frontend/             # Svelte frontend (Vite root)
|   +-- index.html, vite.config.ts, svelte.config.js, tsconfig.json
|   +-- src/
|       +-- App.svelte, main.ts
|       +-- styles/{theme.css,global.css}   # design tokens
|       +-- lib/{ipc.ts,log.ts,theme.ts,components/,views/}
|       +-- assets/       # fonts, license texts
+-- .github/workflows/    # CI: fmt + clippy + test + check + bundle, 3 OSes
+-- docs/
+-- analysis_todo.md
+-- STANDARDS.md          # this workspace's standards
```

Naming: the crates, package, and paths use lower-case `emfit`; the product name
shown to users - window title, executable, About panel - is `EmFit`.

## Build

```powershell
npm run tauri build
```

Produces the platform bundle under `src-tauri/target/release/bundle/` (`.msi`/`.exe`
on Windows, `.dmg`/`.app` on macOS, `.deb`/`.AppImage` on Linux). CI builds and tests
for Windows 11 x64, Linux x86_64, and macOS Apple Silicon on every push and PR.

## License

Proprietary, all rights reserved. See [LICENSE.txt](./LICENSE.txt).
Internal-use grants are extended to Iceberg Forensics LLC and the
Westchester County District Attorney's Office High Tech Crime Bureau.

Bundled third-party components ship under their respective upstream licenses:
Inter font (SIL Open Font License 1.1), Bootstrap Icons (MIT), and the Tauri /
Svelte / Vite toolchain (MIT or Apache-2.0). The About panel surfaces the full
license texts.

---

This project follows [STANDARDS.md](./STANDARDS.md).
