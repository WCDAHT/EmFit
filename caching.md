# EmFit - Scan cache and journal replay

How a rescan stops costing 13.5 seconds. Companion to
[architecture.md](./architecture.md) (which owns the model and the seam) and
[roadmap.md](./roadmap.md) M5 (which owns the milestone). Conventions from
[STANDARDS.md](./STANDARDS.md).

**The one-sentence version:** a finished scan is persisted as a compressed
column snapshot plus the USN position it was taken at, and the next scan of
that volume loads it, replays the journal into a *dirty record list*, re-reads
only those records off the MFT, and rebuilds - the journal is used as a change
list, never as data.

**When it happens:** only when the user asks for a volume to be scanned.
Nothing is loaded at startup, no volume attaches itself to the window
uninvited. Scanning a drive and answering from its snapshot are the same
action with the same result, which is why the cache needs no separate command.

---

## 1. What this buys

Measured on this machine, C: with 3.39M nodes (`bench.jsonl`, 2026-08-21):

| Step | Cost |
|---|---|
| Full MFT sweep + build | 13.5 s, 3.4 GB read |
| Sort ranks warm (7 columns) | 10.4 s background, of which `Path` alone is 7.2 s |
| First search after either | 27 ms |

And measured against a real snapshot of that same C: (`emfit-cli cache
verify`, 3,388,269 nodes, 146 MiB on disk with all four sort orders):

| Step | Cost |
|---|---|
| Snapshot write | ~0.5 s |
| Snapshot read, checksums and validation included | 230 ms |
| Rebuild into an `Index` | 1.27 s |
| Sort orders installed from the snapshot | O(n), no warm-up |

So about 1.5 s of work replaces 13.5 s of sweeping and 10.4 s of warming, plus
whatever the journal replay costs on top. The rebuild is now the dominant term
and the lever for it is sec 5. The BenchLog records `cache_save` beside
`scan_ntfs`, so the ratio keeps measuring itself.

## 2. The shape

Two ideas carry the whole design.

### 2.1 Truth vs derived

Every byte in the cache is one of:

- **Truth** - what only a scan can produce: the node columns, the name arena,
  the filesystem ids, the volume's `$UpCase` table. Losing it means rescanning.
- **Derived** - anything recomputable from truth in O(n): the CSR child array,
  the rollup totals, sort orders. Losing it costs milliseconds.

The container marks which is which. **A derived section that is missing,
stale, or fails its checksum is silently recomputed; a truth section that fails
means the file is discarded and the volume is swept.** That is the whole
compatibility story: no migrations, no version negotiation. A build that meets
a section kind it does not know skips it and rebuilds.

This is also the answer to "the search filters will grow". Every filter the
Everything-style advanced search needs is either a bit test on fields already
in `Node` or one more derived side table keyed by `NodeId` (sec 7). Adding one
is a new section number and a builder function. The format does not move.

### 2.2 The journal is a change list, not a data source

The tempting design - decode `USN_REASON_*` bits and apply each event to the
index - is where drift comes from: reasons arrive coalesced and out of order,
and a journal record carries no size at all. Instead:

```
journal records -> dirty set of MFT record numbers -> targeted MFT reads
                                                   -> current truth
```

Every touched record is re-read from the MFT and re-parsed by the same
`parse_one` the sweep uses. Order does not matter, coalescing does not matter,
a missed nuance in the reason bits does not matter: whatever the record says
now is what lands in the index. Replay is idempotent, which is also why the USN
position is captured *before* the sweep starts (sec 6.1) - changes made during
the scan are replayed harmlessly rather than lost.

Three properties fall out, and they are exactly the cases that usually break
incremental indexes:

- **A record slot reused by a different file** comes out right, because the
  node keyed by that record is replaced wholesale rather than edited.
- **A renamed or moved directory** costs one node update. Parents are held as
  filesystem ids and paths are walked, so the whole subtree follows; NTFS emits
  no per-descendant event and none is needed.
- **A hard link added or removed** comes out right, because every node of a
  record is dropped together and the record's current set of names is inserted
  in their place.

## 3. The container format

`service::cache::format`. Little-endian, sections, checksums.

```
offset  size  field
0       8     magic "EMFITSNP"
8       2     container_version (u16, currently 1)
10      2     endian marker 0x0001 - a mismatch is a hard refuse, never a swap
12      4     section_count
16      8     manifest_offset (JSON, so any build can read it)
24      8     manifest_len
32      -     section table: section_count x 48 bytes
              { kind u32, codec u32, class u32, param u32,
                offset u64, stored_len u64, raw_len u64, hash u64 }
...           the manifest, then the payloads, each 8-byte aligned
```

- `class`: truth or derived. Drives the discard-vs-rebuild decision.
- `param` discriminates sections of one kind - which column a sort order is for.
- `codec`: raw, or LZ4 in independent 1 MiB chunks so that compression and
  decompression both spread across cores and neither needs the section
  resident twice. Every column ships compressed.
- `hash`: xxh3 of the stored bytes, verified on every read.
- The manifest is JSON, not a packed struct, so `cache list` and a human with a
  text editor can both read it, and a file that is not a snapshot fails to
  parse rather than being misread.

**Endianness is decided: little-endian only** (roadmap M5 exit criterion).
Every target platform is LE; a big-endian reader refuses the file and rescans.

Writing goes to `<name>.tmp`, is flushed and `sync_all`ed, and is renamed over
the old snapshot (STANDARDS sec 4.5). A `Writer` dropped without `finish`
removes its temporary, so killing the app mid-scan leaves no corrupt cache -
the criterion is met by construction rather than by care.

### 3.1 Manifest

Carries `schema_version`, the app version, a `VolumeStamp` (fingerprint,
serial, root label, total bytes, cluster size, free bytes), the volume's
`VolumeCaps`, a `ScanStamp` (when, how long, access mode, node count), and the
`JournalStamp` the scan started from.

The fingerprint hashes serial, total bytes and cluster size - deliberately not
the drive letter, because letters are reassigned and a cache keyed to one would
attach itself to whatever volume inherited it. It is also the file name:
`%LOCALAPPDATA%\IcebergForensics\EmFit\data\cache\<fingerprint>.emfit`.

## 4. Sections

| Kind | Class | Bytes/node | Contents |
|---|---|---|---|
| `PARENTS` | truth | 4 | each node's parent, as a node id |
| `FLAGS` | truth | 2 | `EntryFlags` bits |
| `NAME_LENS` | truth | 2 | name length; offsets are derived from the running sum |
| `SIZES`, `ALLOCATED` | truth | 8 each | logical and on-disk bytes |
| `MTIMES`, `CRTIMES` | truth | 8 each | normalized timestamps |
| `FS_IDS` | truth | 8 | the id each entry was pushed under |
| `ARENA` | truth | - | every name, in node order, concatenated |
| `UPCASE` | truth | - | 128 KiB `$UpCase`, so loading needs no volume access |
| `SORT_ORDER` | derived | 4 | node ids in one column's ascending order |

Columns are structure-of-arrays, not a memory dump of `Node`: `Node` has
private fields and no `repr(C)`, so dumping it would freeze a layout the model
is free to change, and SoA compresses several times better besides.

Two things are deliberately **not** stored. The CSR child array and the rollup
totals, because the load path re-runs the builder's link and rollup passes
anyway (sec 5). And alternate data streams, because nothing reads them yet and
the section would cost as much as the arena - a cached load reports no ADS,
which is the one observable difference from a sweep.

`FS_IDS` is the section that makes the round trip possible, and it is why
`Index` now stores each entry's whole id rather than the masked record number.
The extra bits above `FS_OBJECT_MASK` are what tell one hard link from another;
without them the builder's id map would collapse two names of a file into one
node. `Index::native_id` masks on read, so nothing above the model noticed.
The nodes the builder invents - a synthesized root, the `[Unreachable]` folder
- carry reserved ids for the same reason, so they push back through the builder
as themselves instead of being collected a second time.

## 5. Loading

One path, not two: the columns are pushed back through `IndexBuilder` as
`RawEntry` batches, with a replay's deletions filtered out and its new and
changed entries substituted (`snapshot::rebuild`). `finish()` then runs the
same resolve, collect, link and rollup passes it always does.

This is `architecture.md` sec 5 taken at its word - the cache is just another
way to acquire entries - and it is worth the ~370 ms per million nodes it
costs. A second, faster path that assembled `Index` directly would have to
agree with this one about orphan collection, synthetic roots and rollup
arithmetic forever after, and the whole failure mode a cache must not have is
"looks like a scan, is not one". If the rebuild ever becomes the bottleneck the
lever is to store the `Csr` and `Rollup` sections and skip those passes; the
section design already allows it.

Everything a snapshot claims is checked before an entry is pushed: name slices
inside the arena and on character boundaries, parents in range, columns of
equal length, exactly one self-parented root. `Index` slices its arena raw and
only `debug_assert`s its invariants, so a tampered or truncated file has to be
caught in `snapshot::validate` or it panics on a hot path later. The round-trip
tests corrupt one thing at a time and require a clean refusal.

Free space is refreshed from the volume rather than replayed - it changes by
the minute and identifies nothing.

## 6. Journal replay

`service::replay`.

### 6.1 Capture

`scan_volume_inner` queries `FSCTL_QUERY_USN_JOURNAL` **before** the sweep and
puts the id and position in the outcome. Capturing after the sweep would lose
every change made during those thirteen seconds. Capturing before is safe
precisely because replay re-reads records rather than applying events. A volume
with no journal position is never cached: a snapshot nothing could bring up to
date is only a way to show stale data later.

### 6.2 Validate

`usn::read_since` refuses when the journal is inactive, when its id differs
from the manifest, or when the saved position is older than `FirstUsn` - the
journal wrapped. Each of those is a `Gap`, and a gap means sweeping.

A volume with no active journal cannot be replayed. **EmFit does not create
one**: `FSCTL_CREATE_USN_JOURNAL` writes to the volume, which a forensic tool
does not do behind the user's back.

### 6.3 Collect the dirty set

Read from the saved position with every reason bit except `CLOSE`, until
`ERROR_HANDLE_EOF`. Every record named by any event joins one set; the reason
bits are otherwise unread. A delete and a write both mean "look at this record
again".

Escalation, because targeted reads only beat a sweep up to a point. One ioctl
per record costs on the order of ten microseconds, against thirteen seconds for
a whole sweep:

- more than `max(100_000, nodes / 40)` records -> sweep;
- more than eight times that many journal events -> sweep, without even
  finishing the read.

### 6.4 Re-read

**Through the filesystem, never off the platter.** This is the part that is
easy to get wrong and was: a raw read of the volume shows what NTFS has written
down, and NTFS keeps modified metadata in its own cache and flushes it lazily.
The records the journal just named are exactly the ones it has *not* written
down yet, so reading them raw shows a created file's slot still free, a deleted
file's still in use, and a written file's old size - and every one of those
turns into a wrong index. `FSCTL_GET_NTFS_FILE_RECORD` asks NTFS for the record
instead, and it answers from the cache it is about to write down. Same bytes,
same parser, current answer ([`crate::parser::ntfs::live`]).

Three properties of that ioctl decide whether it is safe to build on, and all
three are handled in `live.rs`:

- A record that is **not in use does not fail**. The driver returns the nearest
  in-use record below the one asked for, so the answer has to be checked
  against the request or one file's metadata lands under another's number.
- **The fixup is already applied.** A record read off the volume has the last
  two bytes of every sector replaced by a check value, with the real bytes
  parked in the update sequence array; the driver puts them back before handing
  the record over. Applying the fixup again corrupts two bytes per sector, and
  skipping it on a stored record leaves a check value where data belongs -
  neither mistake announces itself. `RecordForm` carries the distinction, and
  `probe_form` *determines* it at open time by parsing the served record both
  ways and keeping whichever one comes out naming itself. Measured behaviour on
  Windows 10 is `Repaired`; nothing depends on that staying true.
- The ioctl is **proved at open time**, because everything downstream reads "no
  record" as "the file is gone" - an ioctl that failed wholesale would look
  like a volume that emptied itself.

If any of that fails, the replay is blocked and the volume is swept.

`scanner::read_records` then runs the sweep's own machinery over the list:

1. One ioctl per record. There is no seek to amortize - the driver is
   answering out of memory - so the run-coalescing that the block source does
   is not used here. `RecordSource` picks between the two: a disk image or an
   unmounted volume still reads in coalesced 64 KiB runs off the blocks, which
   is what the fixture tests exercise.
2. Records are parsed by `parse_one`, exactly as in a sweep.
3. **Attribute lists are followed.** A base record whose attributes live
   elsewhere is held back by `parse_one`, and a sweep finds those extension
   records by reading everything and matching `base_record()`. A targeted read
   has no sweep, so it walks the other way: the list's own entries name the
   records, those are read in the next round, and an extension record whose
   base was not touched pulls its base in too. The loop settles in two rounds
   for every real record; four are allowed.
4. A record that is free, or in use with no usable name, produces nothing -
   which is exactly how a deletion is observed.

A record that fails its fixup check, or whose signature is wrong, blocks the
replay too. One torn record is not worth guessing about when a sweep is the
correct answer and costs seconds - and a replay that quietly dropped it would
delete a live file from the index.

A base whose `$ATTRIBUTE_LIST` is itself non-resident is the one case a
targeted read cannot finish: where its parts live cannot be known without
reading the list, and emitting the record without them would report a wrong
size. Those records are reported as `opaque` and the whole replay falls back to
a sweep. They are vanishingly rare; if the logs ever say otherwise, that is the
measurement that would justify handling them properly.

### 6.5 Apply

Every record the read *visited* - not merely those the journal named - is
replaced. Nodes are matched by `fs_id & FS_OBJECT_MASK`, so all of a record's
names go together and the fresh set replaces them. Then the rebuild of sec 5
runs, re-rolling every total from scratch. At ~150 ms for three million nodes,
an incremental rollup along the parent chain is not worth the class of bug it
invites - which is the risk roadmap M5 flags.

## 7. Derived indexes, and how a new filter plugs in

Search matching needs no index: the linear pass over the flat array is 27 ms at
3.4M nodes and stays within a frame. What is worth persisting is anything that
costs more to compute than to read - today, the string-keyed sort columns.

**Sort orders are stored, ranks are derived.** `view.rs` sorts with
`ranks[node] = position`; the snapshot stores the inverse, the node ids in
ascending key order, because an order can be *repaired* where a rank table can
only be rebuilt: new nodes binary-search in, deleted ones splice out. That
repair is not implemented yet - a replay that changed anything currently drops
the orders and re-warms in the background, as before - but storing the
invertible form is what leaves the door open.

Which columns: `Name`, `Path`, `Extension`, `Kind`. `Size`, `Allocated` and
`Modified` cost under 300 ms each to build, which is less than reading them
back, so they are recomputed.

The orders are installed only for a **single-volume** view, which is what
scanning one drive produces. A rank is a position in one global ordering, and
interleaving two volumes' orders means comparing their keys - for `Path`,
exactly the seven seconds caching them was meant to avoid. A multi-volume view
warms the ordinary way.

**The snapshot is written the moment the scan finishes, and the orders catch
up.** They have to be separable: the truth costs half a second and the orders
cost ten, and the first version of this waited for both - so closing the window
during the warm-up wrote no cache at all, and a user who did that twice never
got one. Now the truth lands immediately into a table with rows reserved, and
`format::Appender` fills them when the warm-up finishes (`view::order_from_ranks`
recovers an order from a rank table with an integer sort, so nothing is
recomputed). The append is idempotent - a column already in the file is skipped
- and crash-safe: payloads and their rows are synced before `section_count`
grows to cover them, so an interrupted append leaves the file exactly as valid
as it was.

A snapshot without orders is completely usable; the next load simply warms them
itself. An index that came out of a cache with nothing replayed is not
rewritten at all - the file already says that.

What the advanced-search draft (`advanced-search-draft` branch) will need, and
where each lands:

| Filter | Cost today | Index |
|---|---|---|
| name length, attributes, size, dates | O(1) per node | none needed |
| `ext:` | folds a string per node | an extension-id column - integer compare |
| folder depth | walks parents | a depth column, one byte per node |
| child items / files / folders | walks CSR children | direct counts, directories only |
| duplicates (name/size/dates) | a HashMap over every node, per query | a dense class id per node per field, so a multi-field check is a tuple of `u32` instead of hashing strings. The `Name` and `Size` orders already put equal keys adjacent, so building the ids is one linear pass over an order the cache already holds. |
| search the full path | assembles a path per node | a path order plus a length column. Not a path arena; it is bigger than the index. |
| content search | reads the file | not indexable without a content index. Out of scope - it is the one slow filter, and it is slow in Everything too. |

None of those changes the format. That is the point.

## 8. Failure modes

| Situation | Detected by | Response |
|---|---|---|
| Cache absent | no file | sweep, as before |
| Volume reformatted / different disk | fingerprint mismatch | discard, sweep |
| Truncated or tampered file | magic, section checksum, structural validation | discard, sweep |
| Derived section corrupt | checksum, `class == derived` | drop that section, rebuild it |
| Journal wrapped or recreated | journal id, position vs `FirstUsn` | sweep, and say why |
| Journal disabled or unreadable | the query fails | sweep |
| The filesystem will not serve records | the probe in `LiveRecords::open` | sweep |
| A record will not parse | fixup or signature check | sweep |
| Not elevated | the volume will not open | the scan fails as it always did |
| Too much changed | record and event budgets | sweep |
| A non-resident attribute list | `opaque` records | sweep |
| Killed mid-save | the temporary is never renamed | the previous snapshot stands |
| Two instances saving | rename is atomic | last writer wins, both valid |

Every one of those is logged with its reason, and none of them is an error the
user has to act on. The status line says which route a scan took - "loaded from
a 3-hour-old cache; 412 changed record(s) re-read" - so a cached result never
passes itself off as a fresh sweep silently.

### 8.1 A deliberate deviation from STANDARDS 4.5

The standard says SQLite for large data. This is a single immutable array
loaded whole, never queried relationally, with a latency target in
milliseconds. A row store would cost seconds and buy nothing. Custom binary,
recorded here per the standard's own preference for writing the choice down.

## 9. Disk budget

Config carries `cache_enabled` (default on, switchable under Settings >
General) and `cache_budget_mb` (default 2048). After every write the oldest
snapshots are evicted until the directory fits. `emfit-cli cache list` shows
what is there and how old; `cache clear` removes it.

## 10. Where the code lives

```
emfit-core/src/service/cache/
+-- mod.rs        paths, fingerprints, save/load/append/list/evict, the API
+-- format.rs     the container: header, section table, checksums, codecs,
|                 Writer (reserves rows) and Appender (fills them)
+-- manifest.rs   what a snapshot claims about itself
+-- snapshot.rs   Index <-> columns, structural validation, rebuild
emfit-core/src/service/replay.rs           journal -> dirty set -> Patch
emfit-core/src/parser/ntfs/scanner.rs      read_records: targeted reads
emfit-core/src/service/scan.rs             from_cache, wired into scan_volume
emfit-core/src/service/view.rs             build_order / ranks_from_order /
                                           order_from_ranks
src-tauri/src/commands.rs                  install the orders, save after warm-up
```

Nothing above `model/` learns what a cache is, and `model/` learned only that
an entry's id is kept whole.

## 11. State

**Done.** The container, the columns, save and load, structural validation,
LZ4 compression, journal capture and replay, targeted reads with attribute-list
following, the rebuild path, cached sort orders appended after the warm-up, the
CLI's `cache list|verify|clear` and `--no-cache` / `--save-cache`, the settings
toggle, the budget and eviction, and the status line.

`emfit-cli cache verify` loads every snapshot and rebuilds its index, reporting
what that cost. It needs no volume and no elevation, so it exercises everything
about a cached load except the journal replay - against the real file rather
than a fixture. It is how the numbers in sec 1 were measured.

`emfit-cli cache replay --drive C` (elevated) does the other half: it reads the
journal, re-reads what changed, and prints the entries it found and the records
that came back empty, without writing anything back. That is how to check that
a file created, edited, or deleted a moment ago actually reaches the index -
run it twice and it says the same thing both times.

**Tested.** `tests/cache_roundtrip.rs` asserts that a reloaded scan is the same
scan - every node field, every path, every rollup total, and a battery of
queries - plus node-id stability, hard links, the invented folders, patching,
and a hostile-input suite (truncated, forged header, flipped payload bytes).
`tests/targeted_read.rs` asserts the property replay depends on: *a targeted
read of a record produces exactly what a full sweep produced for it*, over
fragmented MFTs, attribute lists in both directions, hard links, 4Kn geometry,
free records, and records past the end of the table.

**Not yet done**, in rough order of value:

1. **A real-volume test.** Everything above runs on synthetic images; the
   replay path has never met a live USN journal, because that needs
   Administrator. The acceptance test is: scan, save, create / delete / rename /
   grow files, scan again, and compare node for node against a forced sweep
   (`--no-cache`). Until that has been run, treat replay as unproven.
2. **Order repair** (sec 7): binary-search new nodes into a stored order
   instead of dropping it. Worth ~10 s of background warming after any replay
   that changed something.
3. **The derived-section registry.** The framework is a section number, a
   builder and a repair function; only sort orders use it so far. The columns
   the advanced search wants (sec 7) plug in here.
4. **ADS in the snapshot**, if anything ever reads them.
5. **A portable scan file** - the same container without a volume fingerprint,
   opened read-only as an "opened scan" (features.md sec 1.6). Deferred: the
   read-only mode it implies is more UI work than the container work.

## 12. Fixed on the way

**The filesystem serves records with the fixup already applied.** The first
attempt at reading through the driver assumed records arrived as stored and
applied the fixup itself, which read as a torn write on every record and
blocked every replay. The form is now determined from a record rather than
assumed - sec 6.4. This one was caught by the open-time probe rather than in
production, which is the argument for having it.

**Replay read the platter, not the filesystem.** The first version re-read
changed records straight off the volume, which is what a sweep does and is
right for a sweep. For records that changed seconds ago it is wrong: NTFS had
not written them down yet. Creating a file and rescanning did not add it (its
record still read as free); editing it and rescanning added it with the wrong
size (a stale `$DATA`); deleting it and rescanning left it in place (the record
still read as in use). Each change appeared exactly one rescan late, once the
lazy writer caught up. Records now come from `FSCTL_GET_NTFS_FILE_RECORD` -
sec 6.4.

**A patched snapshot's sort orders were installed anyway.** A stored order
lists node ids, and a replay that adds or drops anything renumbers them, so the
orders from a snapshot that was patched on the way in point at the wrong rows -
and sorting by one would index past the end of the index. They are now used
only when the replay changed nothing; a changed volume warms from scratch until
order repair (sec 11) lands.

**The snapshot was written too late.** The first version wrote the file at the
end of the sort-rank warm-up, about twelve seconds after a scan finished, so
that the expensive columns could go in with it. Closing the window in the
meantime - which is the normal thing to do once the results are on screen -
wrote nothing, and the next run swept the table again. Two runs in a row could
both sweep and still leave no cache behind. The truth now lands as soon as the
scan does; sec 7 has the split.

`scanner::alias_id(record, i)` returned `record` itself for `i == 0`, and
`owning_slot` can pick any slot - so a record whose best name was slot 1 emitted
the owning entry under `record` and slot 0's alias under the same id, and the
two fought over one entry in the builder's id map. Harmless while nothing keyed
nodes by id; not harmless once the cache does. The counter is now `i + 1`, with
a regression test that no alias of a record can ever equal it.
