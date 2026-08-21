//! Turns a stream of [`RawEntry`] into an [`Index`].
//!
//! Scanners push batches; this accumulates them and, once the scan ends, runs
//! four linear passes to produce the finished index:
//!
//! 1. **resolve** - filesystem parent ids become [`NodeId`]s
//! 2. **collect** - anything not reaching the root goes to a synthetic folder
//! 3. **link** - parent->child edges into CSR form
//! 4. **rollup** - subtree sizes and counts, children before parents
//!
//! Entries may arrive in any order and a parent may arrive after its children,
//! which is why parent ids are parked raw and resolved in one pass at the end.
//! NTFS pushes in MFT order (effectively random); directory walkers push
//! parents first. Neither is privileged. See `architecture.md` sec 5, sec 8.

use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::model::caps::VolumeCaps;
use crate::model::entry::{EntryFlags, RawEntry};
use crate::model::index::{Index, Node, NodeId};
use crate::model::sink::{EntrySink, ScanWarning, WarningLog};
use crate::service::task::CancellationToken;

/// Name given to the folder that collects entries whose parent could not be
/// resolved. Flagged [`EntryFlags::SYNTHETIC`] so nothing mistakes it for a
/// real directory on the volume.
pub const ORPHAN_FOLDER_NAME: &str = "[Unreachable]";

/// Ids reserved for the nodes the builder invents.
///
/// They sit above [`FS_OBJECT_MASK`](crate::model::index::FS_OBJECT_MASK) and
/// above any id a scanner can mint (the
/// hard-link discriminator would need a record number of `0xFFFF_FFFF_FFFC`
/// and a 65535th link to reach them), so a snapshot can push a synthetic node
/// back through this builder like any other entry and get the same tree
/// (`caching.md` sec 5). `u64::MAX` is spoken for by the free-space row
/// ([`crate::service::scan::FREE_SPACE_FS_ID`]), which is synthetic but is a
/// real entry a scan produced rather than one this builder invented.
pub const SYNTHETIC_ROOT_FS_ID: u64 = u64::MAX - 1;
/// Id of the folder that collects unrooted entries. See [`ORPHAN_FOLDER_NAME`].
pub const ORPHAN_FS_ID: u64 = u64::MAX - 2;

/// Maps a filesystem's own ids onto [`NodeId`]s during the build.
///
/// Dense when ids are small and contiguous - NTFS record numbers and ext4
/// inodes are, so a `Vec` makes every lookup an array index. Sparse otherwise,
/// because a `Vec` indexed by ZFS object ids would be mostly holes.
///
/// Build-time only; dropped by [`IndexBuilder::finish`].
enum FsIdMap {
    Dense(Vec<u32>),
    Sparse(HashMap<u64, NodeId>),
}

impl FsIdMap {
    /// Empty slot marker in the dense representation.
    const VACANT: u32 = u32::MAX;

    /// Highest id kept in dense form. Past this the `Vec` would cost more than
    /// the hash map: 64 Mi slots is a 256 MB allocation.
    const DENSE_LIMIT: u64 = 64 << 20;

    fn new() -> Self {
        Self::Dense(Vec::new())
    }

    fn insert(&mut self, fs_id: u64, id: NodeId) {
        if let Self::Dense(slots) = self {
            if fs_id < Self::DENSE_LIMIT {
                let need = fs_id as usize + 1;
                if slots.len() < need {
                    slots.resize(need, Self::VACANT);
                }
                slots[fs_id as usize] = id.get();
                return;
            }
            self.promote();
        }
        match self {
            Self::Sparse(map) => {
                map.insert(fs_id, id);
            }
            Self::Dense(_) => unreachable!("promoted above"),
        }
    }

    fn get(&self, fs_id: u64) -> Option<NodeId> {
        match self {
            Self::Dense(slots) => match slots.get(fs_id as usize).copied() {
                Some(Self::VACANT) | None => None,
                Some(raw) => Some(NodeId::new(raw)),
            },
            Self::Sparse(map) => map.get(&fs_id).copied(),
        }
    }

    /// Move dense contents into a hash map, after an id arrived too large to
    /// index directly.
    fn promote(&mut self) {
        if let Self::Dense(slots) = self {
            let mut map = HashMap::with_capacity(slots.len());
            for (fs_id, &raw) in slots.iter().enumerate() {
                if raw != Self::VACANT {
                    map.insert(fs_id as u64, NodeId::new(raw));
                }
            }
            *self = Self::Sparse(map);
        }
    }
}

/// Accumulates pushed entries and assembles the finished [`Index`].
pub struct IndexBuilder {
    // become the Index
    nodes: Vec<Node>,
    arena: String,
    fs_ids: Vec<u64>,

    // build-time only, all dropped at finish()
    parent_fs: Vec<u64>,
    fs_to_node: FsIdMap,
    root_fs: Option<u64>,

    // policy + diagnostics
    caps: VolumeCaps,
    cancel: CancellationToken,
    warnings: WarningLog,
}

impl IndexBuilder {
    /// A builder for a volume with the given capabilities.
    pub fn new(caps: VolumeCaps, cancel: CancellationToken) -> Self {
        Self {
            nodes: Vec::new(),
            arena: String::new(),
            fs_ids: Vec::new(),
            parent_fs: Vec::new(),
            fs_to_node: FsIdMap::new(),
            root_fs: None,
            caps,
            cancel,
            warnings: WarningLog::new(),
        }
    }

    /// Pre-allocate for a known entry count. Scanners that can estimate - NTFS
    /// knows its MFT record count before reading - should, to avoid regrowing
    /// several hundred megabytes mid-scan.
    pub fn reserve(&mut self, entries: usize) {
        self.nodes.reserve(entries);
        self.parent_fs.reserve(entries);
        if self.caps.has_stable_ids {
            self.fs_ids.reserve(entries);
        }
        // Names average well under 32 bytes; over-reserving the arena costs
        // more than the occasional regrow.
        self.arena.reserve(entries * 16);
    }

    /// Nodes accumulated so far. Useful for progress reporting.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True before the first entry arrives.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Run the four passes and produce the index.
    ///
    /// Warnings are returned alongside rather than stored inside: they describe
    /// the scan, not the data.
    pub fn finish(self) -> (Index, Vec<ScanWarning>) {
        let Self {
            mut nodes,
            mut arena,
            mut fs_ids,
            parent_fs,
            fs_to_node,
            root_fs,
            caps,
            mut warnings,
            ..
        } = self;

        let has_ids = caps.has_stable_ids;

        // Pass 0 - make sure there is a root to hang everything from.
        let root = match root_fs.and_then(|fs| fs_to_node.get(fs)) {
            Some(id) => id,
            None => {
                warnings.push(ScanWarning::SynthesizedRoot);
                synthesize_node(
                    &mut nodes,
                    &mut arena,
                    &mut fs_ids,
                    has_ids,
                    "",
                    SYNTHETIC_ROOT_FS_ID,
                    None, // becomes its own parent
                )
            }
        };

        // Pass 1 - filesystem parent ids become NodeIds. Anything unresolvable
        // is left pointing at itself and picked up by pass 2.
        for (i, node) in nodes.iter_mut().enumerate() {
            let id = NodeId::new(i as u32);
            if id == root {
                node.parent = root;
                continue;
            }
            // A synthesized root has no entry in parent_fs.
            let Some(&parent_fs_id) = parent_fs.get(i) else {
                continue;
            };
            match fs_to_node.get(parent_fs_id) {
                Some(parent) if parent != id => node.parent = parent,
                Some(_) => node.parent = id, // self-parent that is not the root
                None => {
                    warnings.push(ScanWarning::MissingParent {
                        fs_id: fs_ids.get(i).copied().unwrap_or(0),
                        parent_id: parent_fs_id,
                    });
                    node.parent = id;
                }
            }
        }

        // Pass 2 - gather everything that cannot reach the root into one
        // synthetic folder. Two causes, one condition: a parent that never
        // arrived, or a parent cycle. Attaching them to the root instead would
        // assert they live at the top of the volume - a claim the scan never
        // made.
        let unrooted = find_unrooted(&nodes, root);
        if !unrooted.is_empty() {
            warnings.push(ScanWarning::OrphansCollected {
                count: unrooted.len() as u64,
            });
            let folder = synthesize_node(
                &mut nodes,
                &mut arena,
                &mut fs_ids,
                has_ids,
                ORPHAN_FOLDER_NAME,
                ORPHAN_FS_ID,
                Some(root),
            );
            for id in unrooted {
                nodes[id.index()].parent = folder;
            }
        }

        // Pass 3 - CSR child lists.
        let children = link_children(&mut nodes);

        // Pass 4 - subtree totals, children before parents.
        let visited = rollup(&mut nodes, &children, root);
        if visited != nodes.len() {
            warnings.push(ScanWarning::UnreachableNodes {
                count: (nodes.len() - visited) as u64,
            });
        }

        nodes.shrink_to_fit();
        arena.shrink_to_fit();

        let index = Index::from_parts(nodes, arena, children, fs_ids, root, caps);
        (index, warnings.into_vec())
    }
}

impl EntrySink for IndexBuilder {
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()> {
        if self.cancel.is_cancelled() {
            return ControlFlow::Break(());
        }

        for entry in entries {
            if self.nodes.len() >= NodeId::MAX_NODES {
                self.warn(ScanWarning::LimitReached {
                    limit: NodeId::MAX_NODES as u64,
                });
                return ControlFlow::Break(());
            }

            let name = clamp_name(entry.name);
            if self.arena.len() + name.len() > u32::MAX as usize {
                self.warn(ScanWarning::LimitReached {
                    limit: u32::MAX as u64,
                });
                return ControlFlow::Break(());
            }

            let id = NodeId::new(self.nodes.len() as u32);
            let name_off = self.arena.len() as u32;
            self.arena.push_str(name);

            let is_dir = entry.flags.is_directory();
            self.nodes.push(Node {
                // Placeholder: the real parent is resolved in finish(), because
                // it may not have been pushed yet.
                parent: id,
                name_off,
                name_len: name.len() as u16,
                flags: entry.flags.bits(),
                size: entry.size,
                allocated: entry.allocated,
                mtime: entry.times.mtime,
                crtime: entry.times.crtime,
                // Seed the rollup with this node's own contribution; pass 4
                // folds descendants in.
                total_size: entry.size,
                total_alloc: entry.allocated,
                file_count: u32::from(!is_dir),
                dir_count: u32::from(is_dir),
                first_child: 0,
                child_count: 0,
            });

            self.parent_fs.push(entry.parent_id);
            if self.caps.has_stable_ids {
                // The low 48 bits are the filesystem's own number. A scanner
                // that emits several entries for one file - the names of a
                // hard-linked file - distinguishes them in the high bits so
                // each gets its own map slot, while every one of them still
                // reports the single record they all describe.
                self.fs_ids.push(entry.fs_id);
            }
            self.fs_to_node.insert(entry.fs_id, id);

            // The root is the entry that is its own parent - NTFS record 5,
            // ext4 inode 2. A well-formed scan pushes exactly one.
            if entry.parent_id == entry.fs_id {
                self.root_fs = Some(entry.fs_id);
            }
        }

        ControlFlow::Continue(())
    }

    fn warn(&mut self, w: ScanWarning) {
        self.warnings.push(w);
    }
}

/// Trim a name to what `Node::name_len` can address, on a char boundary. Real
/// names are far shorter; this guards against corrupt metadata.
fn clamp_name(name: &str) -> &str {
    const MAX: usize = u16::MAX as usize;
    if name.len() <= MAX {
        return name;
    }
    let mut end = MAX;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Append a node the filesystem never reported. Returns its id.
///
/// `parent` is `None` for a synthesized root, which becomes its own parent.
fn synthesize_node(
    nodes: &mut Vec<Node>,
    arena: &mut String,
    fs_ids: &mut Vec<u64>,
    has_stable_ids: bool,
    name: &str,
    fs_id: u64,
    parent: Option<NodeId>,
) -> NodeId {
    let id = NodeId::new(nodes.len() as u32);
    let name_off = arena.len() as u32;
    arena.push_str(name);

    nodes.push(Node {
        parent: parent.unwrap_or(id),
        name_off,
        name_len: name.len() as u16,
        flags: (EntryFlags::DIRECTORY | EntryFlags::SYNTHETIC).bits(),
        dir_count: 1,
        ..Node::default()
    });

    if has_stable_ids {
        fs_ids.push(fs_id);
    }
    id
}

/// Every node that cannot reach the root by following parent pointers.
///
/// The parent graph is functional - each node has exactly one parent - so a
/// single marking walk settles every node in O(n) total. Three live states:
/// unknown, in-progress, settled. Walking up from any node either reaches a
/// node already known to be rooted (the whole path is rooted) or re-enters the
/// in-progress path (a cycle, so the whole path is unrooted - including the
/// tail that merely leads into it, which cannot reach the root either).
fn find_unrooted(nodes: &[Node], root: NodeId) -> Vec<NodeId> {
    const UNKNOWN: u8 = 0;
    const IN_PROGRESS: u8 = 1;
    const ROOTED: u8 = 2;
    const UNROOTED: u8 = 3;

    let mut state = vec![UNKNOWN; nodes.len()];
    state[root.index()] = ROOTED;

    let mut path: Vec<NodeId> = Vec::new();

    for start in 0..nodes.len() {
        if state[start] != UNKNOWN {
            continue;
        }
        path.clear();
        let mut cur = NodeId::new(start as u32);

        let verdict = loop {
            match state[cur.index()] {
                ROOTED => break ROOTED,
                UNROOTED | IN_PROGRESS => break UNROOTED,
                _ => {}
            }
            state[cur.index()] = IN_PROGRESS;
            path.push(cur);
            cur = nodes[cur.index()].parent;
        };

        for &id in &path {
            state[id.index()] = verdict;
        }
    }

    state
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s == UNROOTED)
        .map(|(i, _)| NodeId::new(i as u32))
        .collect()
}

/// Build the CSR child array and fill each node's `first_child`/`child_count`.
///
/// A counting sort: count children per parent, prefix-sum into starting
/// offsets, then place each node in its parent's run.
fn link_children(nodes: &mut [Node]) -> Vec<NodeId> {
    let mut counts = vec![0u32; nodes.len()];

    // The root is its own parent. Counting that edge would make the root its
    // own child, and the traversal in rollup() would never terminate.
    for (i, node) in nodes.iter().enumerate() {
        let parent = node.parent.index();
        if parent != i {
            counts[parent] += 1;
        }
    }

    let mut acc = 0u32;
    for (node, &count) in nodes.iter_mut().zip(counts.iter()) {
        node.first_child = acc;
        node.child_count = count;
        acc += count;
    }

    // Reuse the count array as the per-parent write cursor.
    let mut cursor = counts;
    for (node, slot) in nodes.iter().zip(cursor.iter_mut()) {
        *slot = node.first_child;
    }

    let mut children = vec![NodeId::new(0); acc as usize];
    for (i, node) in nodes.iter().enumerate() {
        let parent = node.parent.index();
        if parent != i {
            let slot = &mut cursor[parent];
            children[*slot as usize] = NodeId::new(i as u32);
            *slot += 1;
        }
    }

    children
}

/// Fold every subtree's sizes and counts into its ancestors. Returns the number
/// of nodes reached, which should equal `nodes.len()`.
///
/// Preorder places a parent before all its descendants, so walking the preorder
/// **backwards** guarantees every child is final before its parent is read -
/// one pass, no recursion, no per-node bookkeeping.
///
/// Additions saturate: a corrupt volume can report sizes that overflow when
/// summed, and a wrong total beats a panic.
fn rollup(nodes: &mut [Node], children: &[NodeId], root: NodeId) -> usize {
    let mut order: Vec<NodeId> = Vec::with_capacity(nodes.len());
    let mut stack = vec![root];

    while let Some(id) = stack.pop() {
        order.push(id);
        let node = &nodes[id.index()];
        let start = node.first_child as usize;
        let end = start + node.child_count as usize;
        stack.extend_from_slice(&children[start..end]);
    }

    for &id in order.iter().rev() {
        let (total_size, total_alloc, file_count, dir_count, parent) = {
            let node = &nodes[id.index()];
            (
                node.total_size,
                node.total_alloc,
                node.file_count,
                node.dir_count,
                node.parent,
            )
        };
        if parent == id {
            continue; // the root
        }
        let up = &mut nodes[parent.index()];
        up.total_size = up.total_size.saturating_add(total_size);
        up.total_alloc = up.total_alloc.saturating_add(total_alloc);
        up.file_count = up.file_count.saturating_add(file_count);
        up.dir_count = up.dir_count.saturating_add(dir_count);
    }

    order.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_map_round_trips() {
        let mut map = FsIdMap::new();
        map.insert(5, NodeId::new(0));
        map.insert(9000, NodeId::new(1));
        assert_eq!(map.get(5), Some(NodeId::new(0)));
        assert_eq!(map.get(9000), Some(NodeId::new(1)));
        assert_eq!(map.get(7), None);
        assert_eq!(map.get(999_999), None);
        assert!(matches!(map, FsIdMap::Dense(_)));
    }

    #[test]
    fn map_promotes_on_large_id_and_keeps_earlier_entries() {
        let mut map = FsIdMap::new();
        map.insert(5, NodeId::new(0));
        map.insert(FsIdMap::DENSE_LIMIT + 1, NodeId::new(1));
        assert!(matches!(map, FsIdMap::Sparse(_)));
        assert_eq!(map.get(5), Some(NodeId::new(0)));
        assert_eq!(map.get(FsIdMap::DENSE_LIMIT + 1), Some(NodeId::new(1)));
        assert_eq!(map.get(7), None);
    }

    #[test]
    fn clamp_name_leaves_normal_names_alone() {
        assert_eq!(clamp_name("report.pdf"), "report.pdf");
    }

    #[test]
    fn clamp_name_truncates_on_a_char_boundary() {
        let long = "\u{e9}".repeat(u16::MAX as usize); // two bytes each
        let clamped = clamp_name(&long);
        assert!(clamped.len() <= u16::MAX as usize);
        assert!(long.starts_with(clamped));
    }
}
