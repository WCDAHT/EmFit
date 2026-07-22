# EmFit — Architecture

How EmFit is shaped and why. [features.md](./features.md) says *what* it does;
this says *how the pieces fit* so that adding a second filesystem later is a
week's work instead of a rewrite. Conventions come from
[STANDARDS.md](./STANDARDS.md); where this document makes a choice, it cites the
section it is conforming to.

**The one-sentence version:** the data model is deliberately boring and
universal, and all the filesystem-specific weirdness lives behind a single
narrow seam — a scanner that *pushes* plain entries into a builder.

---

## 1. The shape of the problem

Every filesystem has to record: what files exist, what they're named, where they
sit in the tree, and how big they are. They differ almost entirely in **where
they park that information**.

**NTFS** keeps one ~1 KB record per file in one big table (the MFT). The record
holds everything about the file *including a reference to its parent directory*.
Read the table front to back and the tree assembles itself.

**ext4** splits it in two. *Inodes* — fixed-size structs in tables spread across
the disk — hold size, timestamps, and permissions, but carry **no name and no
parent**. Names live in *directory entries* inside the data blocks of the
directory itself; a directory is just a file whose contents are a list of
`(name → inode number)` pairs. Building a tree means starting at the root
(inode 2) and descending.

**FAT32 / exFAT** have no inode table at all. The directory entry *is* the
metadata — name, size, timestamps, first cluster, all in one small record inside
the parent directory. Same descent as ext4, but even sizes only appear as you
walk.

**ZFS / APFS / Btrfs** are copy-on-write object stores. Directories are objects
mapping names to object ids; the object holds size and times. Structurally
ext4-like, but reading them means decompressing blocks, following B-trees, and
coping with snapshots and clones that *share storage*.

| | Where the name lives | Where size/times live | Linkage | Stable file id |
|---|---|---|---|---|
| **NTFS** | the file's own record | the file's own record | child → parent | yes (MFT record #) |
| **ext4** | the parent's directory data | inode table | parent → child | yes (inode #) |
| **exFAT / FAT32** | the parent's directory data | the parent's directory data | parent → child | **no** |
| **ZFS / APFS / Btrfs** | the parent (directory object) | the file's object | parent → child | yes (object id) |

NTFS is the outlier, in the convenient direction. Everything else requires
traversal, and one of them has no stable identity at all.

## 2. The core decision

Ask what the application actually needs per file:

> a name, a parent, a size, an allocated size, some timestamps, a few flags, an identity

That list is identical for every filesystem above. All the variation is in *how
you obtain it*.

**Therefore: don't generalize the data model — generalize the acquisition.**

The data model stays concrete, small, and shared. The plugin seam sits one layer
earlier, at the point where bytes on disk become entries. This is the inverse of
the instinct to build an abstract, extensible node type that every filesystem
specializes; that instinct produces a fat model, duplicated index-building code,
and a slow search.

## 3. Layers

```
  ┌────────────────────────────────────────────────────────┐
  │ frontend/ (Svelte)   virtual list · treemap canvas      │
  └───────────────────────────▲────────────────────────────┘
                              │  typed IPC — row windows, never the index
  ┌───────────────────────────┴────────────────────────────┐
  │ src-tauri/           commands, events, capabilities     │
  └───────────────────────────▲────────────────────────────┘
                              │
  ┌───────────────────────────┴────────────────────────────┐
  │ emfit-core/service/  scan orchestration, search,        │
  │                      treemap layout, export, watch      │
  ├────────────────────────────────────────────────────────┤
  │ emfit-core/model/    Index · Node · arena · CSR         │  ← shared, boring
  │                      IndexBuilder (the EntrySink)       │
  ├────────────────────────────────────────────────────────┤
  │ emfit-core/parser/   FsScanner: ntfs · walker · ext4…   │  ← the seam
  │                      BlockSource: volume · disk · image │
  └────────────────────────────────────────────────────────┘
```

Everything above `model/` is written once and never learns what a filesystem is.
Everything below the seam is free to be as weird as its filesystem demands.

## 4. Two orthogonal axes

A common mistake — and one v1 made — is to fuse "where the bytes come from" with
"how to interpret them". v1's `VolumeIO` enum carries `NtfsVolumeData` inside it,
so reading an image file implies NTFS.

Keep them independent:

- **`BlockSource`** — *where bytes come from.* A mounted volume handle
  (`\\.\C:`), a physical drive plus a partition offset (`\\.\PhysicalDrive0`), or
  a disk image file. Just `read_at(offset, buf)`.
- **`FsScanner`** — *how to interpret them.* NTFS, ext4, exFAT, or the
  OS-mediated directory walker.

The product of the two axes comes free. NTFS-in-an-image and ext4-in-an-image
both work without either side knowing about the other, which matters because a
non-Windows filesystem will almost always arrive as a forensic image rather than
a mounted volume — and an unmounted partition means EmFit must parse the MBR/GPT
itself, since Windows can't report extents for a filesystem it doesn't recognize.

## 5. The seam: scanners push entries

Sketches, not final signatures.

```rust
/// One filesystem reader. Registered explicitly (STANDARDS §4.3).
pub trait FsScanner: Send + Sync {
    fn name(&self) -> &str;                       // "NTFS", "ext4", "Directory walk"
    fn caps(&self) -> VolumeCaps;                 // §6
    fn can_scan(&self, probe: &VolumeProbe) -> bool;   // boot sector / superblock sniff

    fn scan(
        &self,
        target:   &ScanTarget,                    // volume, subtree, or image+partition
        source:   &dyn BlockSource,
        sink:     &mut dyn EntrySink,
        progress: &mut dyn FnMut(Progress),       // service::task::Progress
        cancel:   &CancellationToken,             // service::task::CancellationToken
    ) -> Result<()>;
}

/// Where entries land. The one implementation that matters is IndexBuilder.
pub trait EntrySink {
    /// Copy what you need before returning — entries borrow the scanner's buffer.
    /// Returns Break to stop the scan (cancel, or an early-exit limit).
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()>;

    /// A recoverable problem. Record it and keep scanning.
    fn warn(&mut self, w: ScanWarning);
}

/// What every filesystem can produce. Borrowed, not owned.
pub struct RawEntry<'a> {
    pub fs_id:     u64,      // MFT record #, inode #, or a synthetic counter
    pub parent_id: u64,
    pub name:      &'a str,
    pub size:      u64,      // logical bytes
    pub allocated: u64,      // bytes actually occupied; == size if unknown
    pub times:     Times,    // already normalized — see rule 3
    pub flags:     EntryFlags, // DIRECTORY | HIDDEN | SYSTEM | SYMLINK | SPARSE | COMPRESSED
}
```

How each scanner fills the same struct:

- **NTFS** sweeps the MFT in extent order, parses ~4 MB at a time across rayon
  threads, and pushes a batch per thread-local buffer. Order is arbitrary.
- **ext4** runs two internal phases — preload the inode tables (sequential, fast)
  into an array, then descend directories and join each name against the
  preloaded inode. The seam neither knows nor cares that there were two phases.
- **FAT32 / exFAT** descend directories, minting `fs_id` from a counter because
  there is no real identity to use.
- **The directory walker** (`FindFirstFileEx` with `FindExInfoBasic` +
  `FIND_FIRST_EX_LARGE_FETCH`) descends and pushes. This is the second scanner
  EmFit needs anyway — for FAT, ReFS, and network shares — and it is what
  validates that the seam is in the right place, because its shape is
  parent→child, the opposite of NTFS.

### Why push rather than a pull iterator

Each scanner keeps control of its own loop, and their loops are genuinely
different: NTFS wants one huge sequential read fanned out across threads, FAT
wants recursive descent, ext4 wants preload-then-join. A pull-based
`next() -> Option<Entry>` forces all three into one shape and makes the
parallel NTFS path impossible without an internal queue. The cost of push is
inversion of control, itemized in §9.

### The four contract rules

1. **Batched, not per-file.** `push_batch`, never `push`. STANDARDS §4.3 warns
   against `Box<dyn Trait>` in hot paths; batching amortizes the one virtual
   call across a few thousand entries, which keeps the trait object at the seam
   without paying for it per file.
2. **No ordering guarantee.** A parent may arrive after its children, or never.
   NTFS pushes in table order (effectively random); walkers push parents first.
   Any code that assumes parents-first will work on one scanner and break on the
   other. Parents are resolved in a post-pass, always.
3. **Borrowed, not owned.** `name` points into the scanner's read buffer. The
   sink copies into the arena immediately or loses it. This is what keeps the
   hot path allocation-free.
4. **`Break` stops everything.** Cancellation, "preview the first 1000", and
   early exit are one mechanism. A `#[must_use]` return means a scanner author
   cannot silently ignore it — which is exactly how v1's cancellation flag
   ended up wired to nothing.

## 6. Capabilities, not `if fs == Ntfs`

The genuinely non-portable facts become data the UI reads:

```rust
pub struct VolumeCaps {
    pub case_sensitive:     bool,   // ext4 yes, NTFS no — changes search itself
    pub has_stable_ids:     bool,   // FAT32 false → no open-by-id, no live diffing
    pub has_hard_links:     bool,
    pub has_allocated_size: bool,   // false → hide the "size on disk" column
    pub live_updates:       bool,   // USN journal; no ext4 equivalent
    pub sizes_are_exact:    bool,   // false under compression / dedup / snapshots
    pub path_separator:     char,
    pub root_label:         String, // "C:", "/", "evidence.dd :: part2"
}
```

`case_sensitive` is the one that reaches upward into otherwise-shared code —
search must fold case per the volume's rule, not a global one.

`sizes_are_exact` is the honest answer to copy-on-write filesystems. Under ZFS
or APFS, snapshots and clones share blocks: two files can each truthfully report
1 GB while together occupying 1 GB. There is no model that fixes this. The right
response is to label it in the UI rather than present a confident wrong number.

## 7. The index

Owned by `model/`. Concrete, flat, and never touched by a scanner.

```rust
pub struct Index {
    nodes:      Vec<Node>,     // NodeId is the index into this vec
    arena:      String,        // every name, concatenated, UTF-8
    children:   Vec<NodeId>,   // CSR: node.first_child .. + child_count
    native_ids: Vec<u64>,      // side table; empty when caps.has_stable_ids is false
    caps:       VolumeCaps,
}

pub struct Node {
    parent:      NodeId,   // u32
    name_off:    u32,      // into arena
    name_len:    u16,
    flags:       u16,
    size:        u64,
    allocated:   u64,
    mtime:       i64,      // nanoseconds since the Unix epoch — normalized
    crtime:      i64,
    total_size:  u64,      // rollup, filled after the scan
    total_alloc: u64,
    file_count:  u32,
    dir_count:   u32,
    first_child: u32,
    child_count: u32,
}
```

### Rule 1 — `NodeId` is ours, not the filesystem's

A dense `u32` the builder assigns as entries arrive. The filesystem-native id
lives in a side table, present only when the filesystem has one. v1 used the MFT
record number *as* the array index and hardcoded "root is record 5", which bakes
NTFS into the model's spine. With this rule, FAT32's total absence of file
identity is a `has_stable_ids: false` and nothing more.

The root is whatever node the scanner declares, not a constant.

### Rule 2 — per-filesystem extras go in side tables keyed by `NodeId`

Alternate data streams (NTFS), extended attributes (ext4), snapshot membership
(ZFS) each become an optional side structure that costs zero when the scanner
doesn't produce one. Never widen `Node` to the union of every filesystem's
fields.

The reason is measurable: at 5M files every extra 8 bytes is 40 MB, and — more
importantly — a cache miss on every pass of search, sort, and rollup. **Node
smallness is search speed.** The struct above is ~80 bytes; if search becomes
cache-bound the escape hatches, in order, are: move the rollup fields into a
directory-only side table (only directories need them — a file's total is its
own size), then move `crtime` out, then split the whole thing
structure-of-arrays so a name scan touches only name bytes. Measure first.

### Rule 3 — normalize at the seam, never in the UI

Timestamps are the standing example: NTFS uses 100 ns ticks since 1601, ext4
uses epoch seconds plus nanoseconds, FAT32 uses DOS time at two-second
resolution. Convert once, inside the scanner. v1 stored raw `FILETIME` through
every layer, which is precisely the leak that makes a second filesystem painful.

Same for path assembly: separator and root label come from `VolumeCaps`, never
from a hardcoded `format!("{}:\\{}", drive_letter, …)`.

## 8. Build pipeline

```
 [1] scan          scanner reads, parses, pushes batches      ← parallel inside the scanner
 [2] intern        sink copies names to arena, appends rows   ← single-threaded, trivial work
 [3] resolve       fs_id → NodeId; fix up parent references   ← single pass over the flat array
 [4] link          CSR children: count → prefix-sum → fill    ← two passes, O(n)
 [5] rollup        reverse topological order, one pass        ← O(n)
 [6] publish       Index handed to the service layer as Arc
```

**Keep the sink dumb.** Its whole job in stage 2 is: copy the name, append a
row. All the clever work happens afterward, over flat arrays, where it is cheap
and predictable. A sink that resolves parents, deduplicates, or links children
inline is a sink that has to be locked, and locking is what eats the parallelism
stage 1 just bought.

Stages 3–5 are the specific things v1 got wrong: it linked children by pushing
into per-parent `Vec`s guarded by a linear `contains()` scan — O(k²) per
directory — and then repeated the entire operation a second time in `build()`.
On directories with tens of thousands of entries, that dominated the whole scan.

## 9. Consequences of the push model

Accepted knowingly. Each has a mitigation that must be designed in, not
retrofitted.

| Consequence | Symptom | Mitigation |
|---|---|---|
| **Inverted control** | Cancel does nothing for 8 seconds | `ControlFlow` return from `push_batch`, `#[must_use]` |
| **Sink contention** | Adding threads makes it 15% faster, not 5× | Thread-local batches of a few thousand; dumb sink |
| **Borrowed entries** | Sink can't stash entries for later | Documented: copy now or lose it |
| **Partial failure** | One unreadable directory aborts a 5M-file scan | `sink.warn()`; recoverable → warn and continue, unrecoverable → `Err` |
| **Backwards stack traces** | Can't breakpoint "the 3-millionth entry" | Wrap the sink: `TeeSink`, `ValidatingSink` |
| **No backpressure** | Producer outruns consumer in future streaming | Theoretical today; `ControlFlow` is where it'd go |
| **No iterator composition** | No `.filter().take()` for free | `Break` covers `take`; filtering belongs in the builder anyway |
| **Ordering footgun** | Works on the walker, breaks on NTFS | Contract rule 2; make the builder order-independent |
| **Test scaffolding** | Needs a `RecordingSink` | ~30 lines, once |

The two that cost something real are **sink contention** and **ordering**. Both
are settled by decisions made before the first scanner exists.

## 10. Module map

Following STANDARDS §1 — one concept per file, no `utils.rs`.

```
emfit-core/src/
├── model/
│   ├── index.rs        Index, Node, NodeId, arena + CSR accessors
│   ├── builder.rs      IndexBuilder — the one real EntrySink
│   ├── entry.rs        RawEntry, EntryFlags, Times
│   ├── caps.rs         VolumeCaps
│   └── volume.rs       VolumeProbe, ScanTarget, volume enumeration types
├── parser/
│   ├── mod.rs          FsScanner trait + ScannerRegistry (STANDARDS §4.3)
│   ├── block.rs        BlockSource trait: volume / physical drive / image
│   ├── ntfs/           the primary scanner — MFT sweep, fixups, $Upcase, USN
│   ├── walker.rs       FindFirstFileEx fallback (FAT, ReFS, network)
│   └── ext4/           …when it exists
└── service/
    ├── scan.rs         orchestration: probe → pick scanner → build → publish
    ├── search.rs       query parsing, case folding per caps, result windows
    ├── treemap.rs      squarified layout for a canvas size and drill level
    ├── watch.rs        USN journal live updates (gated on caps.live_updates)
    ├── csv_export.rs   implements service::export::Exporter
    ├── task.rs         (exists) Progress, CancellationToken
    └── config.rs       (exists) TOML config
```

`parser/` is already documented in the scaffold as "input format handling, one
file per format" with a trait-plus-registry pattern for N ≥ 3. Filesystems *are*
our formats; `ScannerRegistry` is `ParserRegistry` with the same explicit,
non-reflective registration.

New `Error` variants belong in `error.rs` per STANDARDS §4.1 — `Volume`,
`Filesystem`, `Unsupported` — each carrying the volume or offset that failed.
`Error::Cancelled` already exists.

**Logging** is `tracing` (STANDARDS §4.2), and the hot path gets none of it. One
span per pipeline stage, events at batch granularity or coarser. v1 formatted a
log line — including cloning every hard-link name — for all 5M records and then
discarded it because the filter was off, taking a global mutex each time. A
`Progress::Tick` is likewise emitted on a time interval, not per entry.

## 11. Adding a filesystem: the checklist

1. Add `parser/<fs>/` implementing `FsScanner`.
2. Fill `RawEntry` for every file and directory, normalizing timestamps.
3. Declare `VolumeCaps` honestly — especially `case_sensitive`,
   `has_stable_ids`, and `sizes_are_exact`.
4. Implement `can_scan` against the superblock / boot sector / partition type.
5. Register it in `ScannerRegistry::default()`.
6. Add a fixture image under `emfit-core/tests/fixtures/` and an integration
   test asserting a known tree (STANDARDS §5.1: every new format gets a parser
   test with a real sample).

Nothing above `model/` changes. If a step forces you to touch `Index`, `Node`,
search, treemap, or export, the seam is in the wrong place and that is the bug
to fix — not the model.

## 12. Anticipated scanners, roughly by cost

| Scanner | Cost | Notes |
|---|---|---|
| Directory walker | trivial | Needed anyway for FAT/ReFS/network. Validates the seam. |
| exFAT / FAT32 | easy | Small spec; no file identity; traversal only. |
| ext4 | moderate | Extent-tree decoding for directory data is the bulk of it. |
| Btrfs / APFS | hard | B-trees throughout; snapshots and clones. |
| ZFS | very hard | Compression, RAIDZ reconstruction, dedup. Stresses the model itself (§6). |

Scan performance targets are **per scanner**. "Sequential read at disk
bandwidth" is a property of NTFS's layout, not a promise the architecture can
make for filesystems that must traverse directories to enumerate.

## 13. What is deliberately not decided yet

- **The `FsScanner` trait is not written until the second scanner exists.** Build
  NTFS directly, but hold one discipline absolutely: *the NTFS code never touches
  `Vec<Node>` — it only calls `builder.push_batch(...)`.* That single rule is the
  whole difference between extracting the trait in an afternoon and rewriting the
  core. The trait gets extracted when the directory walker lands.
- **Structure-of-arrays vs array-of-structs** for `Node`: start AoS, measure,
  split only if search is cache-bound.
- **Where the scan file format lands** (features.md §1.6). It is close to a
  memory dump of `Index`, but versioning and endianness need deciding before it
  is written to disk and shared between machines.
- **Whether scanners ever run out-of-process.** Attractive for forensics
  (sandboxing an untrusted image parser) and it fits the seam exactly — the
  entry stream serializes cleanly. Not needed for NTFS.
