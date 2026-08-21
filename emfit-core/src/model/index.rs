//! The scanned volume, flat.
//!
//! One `Vec<Node>` addressed by [`NodeId`], one string arena holding every
//! name, one CSR array holding every parent->child edge. No per-node
//! allocations, no hash maps, no pointers. Search, sort, and rollup are linear
//! scans over contiguous memory, which is where the speed comes from.
//!
//! Built exclusively by [`IndexBuilder`]; immutable afterward. See
//! `architecture.md` sec 7.
//!
//! [`IndexBuilder`]: crate::model::builder::IndexBuilder

use crate::model::caps::VolumeCaps;
use crate::model::entry::{EntryFlags, Times};

/// A position in [`Index::nodes`].
///
/// Assigned by the builder in arrival order - deliberately **not** the
/// filesystem's own number. The MFT record number or inode lives in a side
/// table ([`Index::native_id`]), because FAT32 has no such number at all, ZFS
/// object ids are sparse 64-bit values that would make a `Vec` index
/// nonsensical, and scanning two volumes at once would make MFT record 5
/// ambiguous.
///
/// The newtype is not ceremony: [`Node`] holds two different kinds of `u32`
/// index - `parent` into `nodes`, `first_child` into `children` - and mixing
/// them compiles fine while producing garbage.
/// `Default` is node zero. Meaningful only inside the builder, where every
/// defaulted [`Node`] has its parent overwritten before the index is handed
/// out.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(transparent)]
pub struct NodeId(u32);

impl NodeId {
    /// Largest addressable node count. Well beyond any real volume; the
    /// builder refuses entries past it rather than wrapping.
    pub const MAX_NODES: usize = u32::MAX as usize;

    /// Wrap a raw index.
    #[inline]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Unwrap for use as a slice index.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The raw value, for serialization.
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One file or directory.
///
/// Fields are `pub(crate)` rather than public: the service layer reads them
/// directly on hot paths, but the structural ones (`name_off`, `name_len`,
/// `first_child`, `child_count`) index into sibling arrays and are only valid
/// as a set. Outside the crate, go through [`Index`].
///
/// Size matters more than it looks. At five million files every extra eight
/// bytes is forty megabytes and, worse, a cache miss on every pass of search,
/// sort, and rollup. Before widening this struct, read `architecture.md` sec 7.
#[derive(Debug, Clone, Default)]
pub struct Node {
    /// Containing directory. The root is its own parent, which is how upward
    /// walks terminate without an `Option`.
    pub(crate) parent: NodeId,
    /// Byte offset of this node's name in [`Index::arena`].
    pub(crate) name_off: u32,
    /// Length of the name in bytes (not chars).
    pub(crate) name_len: u16,
    /// Attribute bits, as [`EntryFlags`].
    pub(crate) flags: u16,
    /// Logical size. Zero for directories.
    pub(crate) size: u64,
    /// Bytes occupied on disk.
    pub(crate) allocated: u64,
    /// Last modification, nanoseconds since the Unix epoch.
    pub(crate) mtime: i64,
    /// Creation, nanoseconds since the Unix epoch.
    pub(crate) crtime: i64,
    /// This node plus every descendant. Equals `size` for files.
    pub(crate) total_size: u64,
    /// Allocated equivalent of `total_size`.
    pub(crate) total_alloc: u64,
    /// Files in this subtree, counting self when this is a file.
    pub(crate) file_count: u32,
    /// Directories in this subtree, counting self when this is a directory.
    pub(crate) dir_count: u32,
    /// Start of this node's run in [`Index::children`].
    pub(crate) first_child: u32,
    /// Length of that run.
    pub(crate) child_count: u32,
}

impl Node {
    /// Containing directory. The root returns itself.
    #[inline]
    pub fn parent(&self) -> NodeId {
        self.parent
    }

    /// Attribute bits.
    #[inline]
    pub fn flags(&self) -> EntryFlags {
        EntryFlags::from_bits(self.flags)
    }

    /// Convenience for the check made on nearly every node.
    #[inline]
    pub fn is_directory(&self) -> bool {
        self.flags().is_directory()
    }

    /// True for the handful of nodes the builder invented rather than read
    /// from the filesystem. See [`EntryFlags::SYNTHETIC`].
    #[inline]
    pub fn is_synthetic(&self) -> bool {
        self.flags().is_synthetic()
    }

    /// True for an extra hard-link name whose bytes are counted under another
    /// entry. Its [`Node::allocated`] is zero for that reason, not because the
    /// file is empty. See [`EntryFlags::ALIAS`].
    #[inline]
    pub fn is_alias(&self) -> bool {
        self.flags().is_alias()
    }

    /// Logical size. Zero for directories - use [`Node::total_size`] for the
    /// number a user expects to see next to a folder.
    #[inline]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes occupied on disk.
    #[inline]
    pub fn allocated(&self) -> u64 {
        self.allocated
    }

    /// Timestamps, nanoseconds since the Unix epoch. Zero means unknown.
    #[inline]
    pub fn times(&self) -> Times {
        Times {
            mtime: self.mtime,
            crtime: self.crtime,
        }
    }

    /// This node plus every descendant.
    #[inline]
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Allocated equivalent of [`Node::total_size`].
    #[inline]
    pub fn total_allocated(&self) -> u64 {
        self.total_alloc
    }

    /// Files in this subtree.
    #[inline]
    pub fn file_count(&self) -> u32 {
        self.file_count
    }

    /// Directories in this subtree.
    #[inline]
    pub fn dir_count(&self) -> u32 {
        self.dir_count
    }

    /// Number of direct children.
    #[inline]
    pub fn child_count(&self) -> u32 {
        self.child_count
    }
}

/// A scanned volume: every file and directory, flat and immutable.
pub struct Index {
    /// Every node. [`NodeId`] indexes this.
    nodes: Vec<Node>,
    /// Every name, concatenated. Nodes hold `(offset, length)` into it.
    arena: String,
    /// Every parent->child edge, grouped by parent (CSR). A node's children are
    /// `children[first_child .. first_child + child_count]`.
    children: Vec<NodeId>,
    /// Filesystem-native ids, parallel to `nodes`. Empty when the volume has
    /// no stable ids; zero for synthetic nodes.
    native_ids: Vec<u64>,
    /// The volume root. Its parent is itself.
    root: NodeId,
    /// What this volume can and cannot report.
    caps: VolumeCaps,
}

impl Index {
    /// Assemble from a completed build.
    ///
    /// Not public API - [`IndexBuilder::finish`] is the only caller, and it is
    /// what guarantees the invariants this type relies on: arena offsets in
    /// range, CSR runs disjoint and covering, every node reachable from
    /// `root`.
    ///
    /// [`IndexBuilder::finish`]: crate::model::builder::IndexBuilder::finish
    pub(crate) fn from_parts(
        nodes: Vec<Node>,
        arena: String,
        children: Vec<NodeId>,
        native_ids: Vec<u64>,
        root: NodeId,
        caps: VolumeCaps,
    ) -> Self {
        debug_assert!(root.index() < nodes.len(), "root out of range");
        debug_assert!(
            native_ids.is_empty() || native_ids.len() == nodes.len(),
            "native_ids must be empty or parallel to nodes"
        );
        debug_assert_eq!(
            children.len(),
            nodes.len().saturating_sub(1),
            "every node but the root is exactly one node's child"
        );

        Self {
            nodes,
            arena,
            children,
            native_ids,
            root,
            caps,
        }
    }

    /// The volume root.
    #[inline]
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// What this volume can and cannot report.
    #[inline]
    pub fn caps(&self) -> &VolumeCaps {
        &self.caps
    }

    /// Total node count, files and directories together.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Always false in practice - a built index has at least a root.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// One node.
    ///
    /// # Panics
    /// If `id` did not come from this index.
    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    /// Every node, in id order. The basis of search and sort: one linear scan
    /// over contiguous memory.
    #[inline]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Every node id, in order.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + use<> {
        (0..self.nodes.len() as u32).map(NodeId::new)
    }

    /// This node's name - the file name alone, not a path.
    #[inline]
    pub fn name(&self, id: NodeId) -> &str {
        let node = self.node(id);
        let start = node.name_off as usize;
        &self.arena[start..start + node.name_len as usize]
    }

    /// This node's direct children.
    #[inline]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        let node = self.node(id);
        let start = node.first_child as usize;
        &self.children[start..start + node.child_count as usize]
    }

    /// The filesystem's own identifier - MFT record number, inode number.
    ///
    /// `None` when the volume has no stable ids ([`VolumeCaps::has_stable_ids`])
    /// or the node is synthetic.
    ///
    /// [`VolumeCaps::has_stable_ids`]: crate::model::caps::VolumeCaps::has_stable_ids
    pub fn native_id(&self, id: NodeId) -> Option<u64> {
        if self.native_ids.is_empty() || self.node(id).is_synthetic() {
            return None;
        }
        self.native_ids.get(id.index()).copied()
    }

    /// Ancestors from this node's parent up to and including the root.
    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut cur = id;
        let mut done = id == self.root;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            cur = self.node(cur).parent;
            if cur == self.root {
                done = true;
            }
            Some(cur)
        })
    }

    /// Full display path, assembled from [`VolumeCaps::root_label`] and
    /// [`VolumeCaps::path_separator`] - never a hardcoded drive letter or
    /// backslash, so the same code serves `C:\Users\file.txt` and
    /// `/home/user/file.txt`.
    ///
    /// [`VolumeCaps::root_label`]: crate::model::caps::VolumeCaps::root_label
    /// [`VolumeCaps::path_separator`]: crate::model::caps::VolumeCaps::path_separator
    pub fn path(&self, id: NodeId) -> String {
        let sep = self.caps.path_separator;

        let mut parts: Vec<&str> = Vec::new();
        let mut cur = id;
        // The builder guarantees an acyclic parent chain; the bound is a
        // backstop against a hand-constructed index, not an expected path.
        for _ in 0..self.nodes.len() {
            if cur == self.root {
                break;
            }
            parts.push(self.name(cur));
            cur = self.node(cur).parent;
        }

        let mut out = self.caps.root_label.clone();
        for part in parts.iter().rev() {
            if !out.is_empty() && !out.ends_with(sep) {
                out.push(sep);
            }
            out.push_str(part);
        }
        out
    }
}

impl std::fmt::Debug for Index {
    /// Shape only - an index holds millions of nodes and a multi-megabyte
    /// arena, and no debug dump should ever try to print them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("nodes", &self.nodes.len())
            .field("arena_bytes", &self.arena.len())
            .field("root", &self.root)
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}
