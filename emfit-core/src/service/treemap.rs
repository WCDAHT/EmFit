//! Squarified treemap layout, computed in Rust (features.md §9).
//!
//! The webview never lays out rectangles: it sends a canvas size and a drill
//! point, and gets back a **flat list of rectangles** to paint. Laying out
//! hundreds of thousands of nested rectangles in JS is not viable; painting a
//! precomputed list on a canvas is.
//!
//! # Geometry is allocated bytes, always
//!
//! Rectangles are sized by **allocated** size, not logical size — confirmed
//! against WizTree (features.md §4.2). A folder holding one 100 MB file under
//! four hard-link names draws one 100 MB rectangle, not four: hard-link
//! aliases carry zero allocated bytes and vanish from the geometry, sparse
//! and compressed files occupy their real footprint, and the synthetic
//! free-space row fills the volume out to its capacity. Labels may still
//! show logical size; only the geometry is allocated.
//!
//! # Cost model
//!
//! Recursion stops when a rectangle drops below [`TreemapOptions::min_area`]
//! or [`TreemapOptions::max_depth`], so the output is bounded by *pixels*,
//! not by index size — a 5M-file volume and a 5K-file volume produce
//! similarly sized lists. That is what keeps any drill level under the
//! 100 ms budget (roadmap M3).

use crate::model::index::{Index, NodeId};
use crate::service::filetype::FileKind;
use crate::service::view::human_size;

/// The `id` carried by an aggregate ("N smaller items") rectangle — not a
/// real node; actions must ignore it.
pub const AGGREGATE_ID: u32 = u32::MAX;

/// Directories whose inner area is below this many px² are not subdivided;
/// they paint as solid blocks instead (children would be sub-pixel).
const RECURSE_MIN_AREA: f64 = 32.0;

/// One rectangle to paint. Coordinates are canvas pixels, origin top-left.
#[derive(Debug, Clone, PartialEq)]
pub struct TreemapRect {
    pub vol: u16,
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 0 = the drill root(s); leaves are deepest.
    pub depth: u8,
    pub is_dir: bool,
    /// The free-space row and other builder-invented nodes.
    pub synthetic: bool,
    /// `--category-N` slot for color-by-type; 0 = neutral.
    pub category: u8,
    /// Which depth-1 subtree of the drill root this rectangle falls under —
    /// the key for color-by-folder. Depth-0 rects use their own ordinal.
    pub branch: u16,
    /// This directory rect reserved [`TreemapOptions::dir_header_px`] at its
    /// top for the name strip; its children are laid out below it.
    pub headed: bool,
    /// This directory's children were laid out inside it. False for a
    /// directory too small (or too deep) to subdivide — paint those as solid
    /// blocks, since nothing tiles them.
    pub expanded: bool,
    /// A tail of items too small to place individually, folded into one
    /// rectangle so the parent tiles completely. `id` is [`AGGREGATE_ID`].
    pub aggregate: bool,
    pub name: String,
    /// Logical bytes (label), and the allocated bytes the geometry used.
    pub size: u64,
    pub allocated: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TreemapOptions {
    pub width: f32,
    pub height: f32,
    /// How deep to nest below the drill root.
    pub max_depth: u8,
    /// The smallest rectangle a child is placed at individually, in px².
    /// Children below it are **not dropped** — they fold into one aggregate
    /// rectangle per directory, so the space is always painted.
    pub min_area: f32,
    /// At most this many individually-placed children per directory; the
    /// rest aggregate. Bounds the IPC payload on pathological flat folders.
    pub max_children: usize,
    /// Height reserved at the top of a directory rect for its name strip
    /// (`name\ (999 GB)`), when the rect is tall enough to afford it —
    /// children tile the space **below** the strip, WizTree-style.
    pub dir_header_px: f32,
    /// Padding between a directory's border and its children, on every
    /// side. This is the only spacing in the map: files tile edge to edge,
    /// so the padding is what keeps each folder's border readable.
    pub dir_padding_px: f32,
}

impl Default for TreemapOptions {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 640.0,
            max_depth: 6,
            min_area: 2.0,
            max_children: 2000,
            dir_header_px: 14.0,
            dir_padding_px: 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn area(&self) -> f64 {
        self.w * self.h
    }

    fn shorter(&self) -> f64 {
        self.w.min(self.h)
    }

    /// Shrink by `px` on every side, for the nested-container look.
    fn inset(&self, px: f64) -> Rect {
        let px = px.min(self.w / 2.0).min(self.h / 2.0);
        Rect {
            x: self.x + px,
            y: self.y + px,
            w: self.w - 2.0 * px,
            h: self.h - 2.0 * px,
        }
    }
}

/// Lay out the treemap for the current drill level.
///
/// `drill: None` maps every scanned volume into one canvas, split by volume
/// size (features.md §4.2 multi-volume). `Some((vol, id))` fills the canvas
/// with that node's subtree.
pub fn layout(
    indices: &[&Index],
    drill: Option<(u16, u32)>,
    options: &TreemapOptions,
) -> Vec<TreemapRect> {
    let mut out = Vec::new();
    if options.width < 1.0 || options.height < 1.0 {
        return out;
    }
    let canvas = Rect {
        x: 0.0,
        y: 0.0,
        w: f64::from(options.width),
        h: f64::from(options.height),
    };

    match drill {
        Some((vol, id)) => {
            let Some(index) = indices.get(vol as usize) else {
                return out;
            };
            let at = emit(&mut out, vol, index, NodeId::new(id), canvas, 0, 0);
            maybe_descend(
                index,
                vol,
                NodeId::new(id),
                at,
                canvas,
                1,
                None,
                options,
                &mut out,
            );
        }
        None => {
            // Every volume in one map, weighted by what it holds.
            let items: Vec<(usize, f64)> = indices
                .iter()
                .enumerate()
                .map(|(slot, index)| {
                    let root = index.node(index.root());
                    (slot, root.total_allocated() as f64)
                })
                .filter(|&(_, weight)| weight > 0.0)
                .collect();

            squarify(&items, canvas, |&(slot, _), rect| {
                let index = indices[slot];
                let vol = slot as u16;
                let at = emit(&mut out, vol, index, index.root(), rect, 0, slot as u16);
                maybe_descend(
                    index,
                    vol,
                    index.root(),
                    at,
                    rect,
                    1,
                    Some(slot as u16),
                    options,
                    &mut out,
                );
            });
        }
    }
    out
}

/// Subdivide an emitted directory rect when depth and size allow, marking
/// `expanded`/`headed` on it accordingly. A directory left unexpanded paints
/// as a solid block, so no space is ever blank.
#[allow(
    clippy::too_many_arguments,
    reason = "internal recursion; bundling these into a context struct would only rename the arguments"
)]
fn maybe_descend(
    index: &Index,
    vol: u16,
    id: NodeId,
    at: usize,
    rect: Rect,
    child_depth: u8,
    branch: Option<u16>,
    options: &TreemapOptions,
    out: &mut Vec<TreemapRect>,
) {
    if !index.node(id).is_directory() {
        return;
    }
    let (inner, headed) = child_area(rect, options);
    if child_depth > options.max_depth || inner.area() < RECURSE_MIN_AREA {
        return; // stays unexpanded: painted solid by the frontend
    }
    out[at].headed = headed;
    out[at].expanded = true;
    descend(index, vol, id, inner, child_depth, branch, options, out);
}

/// Where a directory's children go: inside the folder padding, below the
/// name strip when the rect can afford one.
fn child_area(rect: Rect, options: &TreemapOptions) -> (Rect, bool) {
    let inner = rect.inset(f64::from(options.dir_padding_px).max(1.0));
    let header = f64::from(options.dir_header_px);
    if header > 0.0 && inner.h >= header + 10.0 && inner.w >= 32.0 {
        (
            Rect {
                x: inner.x,
                y: inner.y + header,
                w: inner.w,
                h: inner.h - header,
            },
            true,
        )
    } else {
        (inner, false)
    }
}

/// What one tile of a directory is: a real child, or the folded-up tail of
/// children too small (or too many) to place individually.
enum Tile {
    Node(NodeId),
    Rest {
        count: u64,
        size: u64,
        allocated: u64,
    },
}

/// Lay out `dir`'s children into `rect`, tiling it **completely**: children
/// below [`TreemapOptions::min_area`] (or past
/// [`TreemapOptions::max_children`]) fold into one aggregate rectangle
/// instead of being dropped, so a WizTree-style map never shows blank space.
#[allow(
    clippy::too_many_arguments,
    reason = "internal recursion; bundling these into a context struct would only rename the arguments"
)]
fn descend(
    index: &Index,
    vol: u16,
    dir: NodeId,
    rect: Rect,
    depth: u8,
    inherited_branch: Option<u16>,
    options: &TreemapOptions,
    out: &mut Vec<TreemapRect>,
) {
    // Children with real footprint, largest first — squarify requires
    // descending weights. Hard-link aliases (allocated 0) drop out here,
    // which is exactly the one-owner accounting the geometry needs.
    let mut children: Vec<(NodeId, f64)> = index
        .children(dir)
        .iter()
        .map(|&child| {
            let node = index.node(child);
            let weight = if node.is_directory() {
                node.total_allocated()
            } else {
                node.allocated()
            };
            (child, weight as f64)
        })
        .filter(|&(_, weight)| weight > 0.0)
        .collect();
    if children.is_empty() {
        return;
    }
    children.sort_by(|a, b| b.1.total_cmp(&a.1));

    // Split into individually-placed children and the aggregated tail.
    let total: f64 = children.iter().map(|(_, w)| w).sum();
    let scale = rect.area() / total;
    let min_area = f64::from(options.min_area);

    let mut tiles: Vec<(Tile, f64)> = Vec::new();
    let mut rest_weight = 0.0f64;
    let mut rest = (0u64, 0u64, 0u64); // count, size, allocated
    for (i, &(child, weight)) in children.iter().enumerate() {
        if i < options.max_children && weight * scale >= min_area {
            tiles.push((Tile::Node(child), weight));
        } else {
            let node = index.node(child);
            rest_weight += weight;
            rest.0 += 1;
            rest.1 += if node.is_directory() {
                node.total_size()
            } else {
                node.size()
            };
            rest.2 += if node.is_directory() {
                node.total_allocated()
            } else {
                node.allocated()
            };
        }
    }
    if rest_weight > 0.0 {
        tiles.push((
            Tile::Rest {
                count: rest.0,
                size: rest.1,
                allocated: rest.2,
            },
            rest_weight,
        ));
        // The tail's sum can outweigh individual children; keep the list in
        // the descending order squarify expects.
        tiles.sort_by(|a, b| b.1.total_cmp(&a.1));
    }

    squarify(&tiles, rect, |(tile, _), child_rect| {
        // Depth-1 children of the drill root define the folder-color groups;
        // everything deeper inherits.
        let branch = inherited_branch.unwrap_or(out.len() as u16);
        match tile {
            Tile::Node(child) => {
                let at = emit(out, vol, index, *child, child_rect, depth, branch);
                maybe_descend(
                    index,
                    vol,
                    *child,
                    at,
                    child_rect,
                    depth + 1,
                    Some(branch),
                    options,
                    out,
                );
            }
            Tile::Rest {
                count,
                size,
                allocated,
            } => out.push(TreemapRect {
                vol,
                id: AGGREGATE_ID,
                x: child_rect.x as f32,
                y: child_rect.y as f32,
                w: child_rect.w as f32,
                h: child_rect.h as f32,
                depth,
                is_dir: false,
                synthetic: false,
                category: 0,
                branch,
                headed: false,
                expanded: false,
                aggregate: true,
                name: format!("{count} smaller items ({})", human_size(*allocated)),
                size: *size,
                allocated: *allocated,
            }),
        }
    });
}

/// Push one rectangle; returns its position so the caller can mark `headed`.
fn emit(
    out: &mut Vec<TreemapRect>,
    vol: u16,
    index: &Index,
    id: NodeId,
    rect: Rect,
    depth: u8,
    branch: u16,
) -> usize {
    let node = index.node(id);
    let name = if id == index.root() {
        index.caps().root_label.clone()
    } else {
        index.name(id).to_string()
    };
    let kind = FileKind::classify(&name, node.is_directory());
    out.push(TreemapRect {
        vol,
        id: id.get(),
        x: rect.x as f32,
        y: rect.y as f32,
        w: rect.w as f32,
        h: rect.h as f32,
        depth,
        is_dir: node.is_directory(),
        synthetic: node.is_synthetic(),
        category: kind.category_slot(),
        branch,
        headed: false,
        expanded: false,
        aggregate: false,
        size: if node.is_directory() {
            node.total_size()
        } else {
            node.size()
        },
        allocated: if node.is_directory() {
            node.total_allocated()
        } else {
            node.allocated()
        },
        name,
    });
    out.len() - 1
}

/// The squarified treemap algorithm (Bruls, Huizing, van Wijk).
///
/// Items must be sorted by descending weight. Each item's rectangle is
/// handed to `place`; areas are proportional to weights and exactly tile
/// `rect` (up to floating-point rounding).
fn squarify<T>(items: &[(T, f64)], rect: Rect, mut place: impl FnMut(&(T, f64), Rect)) {
    let total: f64 = items.iter().map(|(_, w)| w).sum();
    if total <= 0.0 || rect.area() <= 0.0 {
        return;
    }
    let scale = rect.area() / total;

    let mut remaining = rect;
    let mut i = 0;

    while i < items.len() {
        // Grow the row while doing so improves the worst aspect ratio.
        let side = remaining.shorter();
        let mut row_end = i + 1;
        let mut row_area = items[i].1 * scale;
        let mut best = worst_ratio(&items[i..row_end], scale, row_area, side);

        while row_end < items.len() {
            let next_area = row_area + items[row_end].1 * scale;
            let next = worst_ratio(&items[i..=row_end], scale, next_area, side);
            if next > best {
                break;
            }
            row_end += 1;
            row_area = next_area;
            best = next;
        }

        // Lay the row along the shorter side of what remains.
        let horizontal = remaining.w >= remaining.h;
        let thickness = if side > 0.0 { row_area / side } else { 0.0 };
        let mut along = if horizontal { remaining.y } else { remaining.x };

        for item in &items[i..row_end] {
            let length = if thickness > 0.0 {
                item.1 * scale / thickness
            } else {
                0.0
            };
            let cell = if horizontal {
                Rect {
                    x: remaining.x,
                    y: along,
                    w: thickness,
                    h: length,
                }
            } else {
                Rect {
                    x: along,
                    y: remaining.y,
                    w: length,
                    h: thickness,
                }
            };
            place(item, cell);
            along += length;
        }

        if horizontal {
            remaining.x += thickness;
            remaining.w -= thickness;
        } else {
            remaining.y += thickness;
            remaining.h -= thickness;
        }
        i = row_end;
    }
}

/// The worst (largest) aspect ratio a row would have at this thickness.
fn worst_ratio<T>(row: &[(T, f64)], scale: f64, row_area: f64, side: f64) -> f64 {
    if row_area <= 0.0 || side <= 0.0 {
        return f64::MAX;
    }
    let thickness = row_area / side;
    let mut worst = 1.0f64;
    for (_, weight) in row {
        let length = weight * scale / thickness;
        if length <= 0.0 {
            return f64::MAX;
        }
        let ratio = (thickness / length).max(length / thickness);
        worst = worst.max(ratio);
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::builder::IndexBuilder;
    use crate::model::entry::{EntryFlags, RawEntry, Times};
    use crate::model::sink::EntrySink;
    use crate::service::task::CancellationToken;

    fn file(fs_id: u64, parent: u64, name: &str, allocated: u64) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: parent,
            name,
            size: allocated,
            allocated,
            times: Times::default(),
            flags: EntryFlags::empty(),
        }
    }

    fn dir(fs_id: u64, parent: u64, name: &str) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: parent,
            name,
            size: 0,
            allocated: 0,
            times: Times::default(),
            flags: EntryFlags::DIRECTORY,
        }
    }

    /// root(5) → docs(16){a:600, b:200}, big.iso:1200
    fn index() -> Index {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let _ = b.push_batch(&[
            dir(5, 5, ""),
            dir(16, 5, "docs"),
            file(17, 16, "a.pdf", 600),
            file(18, 16, "b.pdf", 200),
            file(19, 5, "big.iso", 1200),
        ]);
        b.finish().0
    }

    fn options() -> TreemapOptions {
        TreemapOptions {
            width: 1000.0,
            height: 500.0,
            max_depth: 6,
            min_area: 1.0,
            ..TreemapOptions::default()
        }
    }

    #[test]
    fn areas_are_proportional_to_allocated_bytes() {
        let index = index();
        let rects = layout(&[&index], None, &options());

        let big = rects.iter().find(|r| r.name == "big.iso").unwrap();
        let docs = rects.iter().find(|r| r.name == "docs").unwrap();

        // big.iso is 1200 of 2000 total → 60% of the canvas (children tile
        // the root's inner area, which loses the 1px padding and the 14px
        // name strip — a few percent on a 500px canvas).
        let canvas_area = 1000.0 * 500.0;
        let big_share = (big.w * big.h) / canvas_area;
        assert!(
            (big_share - 0.6).abs() < 0.04,
            "big.iso covers {big_share:.3} of the canvas, wanted ~0.60"
        );
        let docs_share = (docs.w * docs.h) / canvas_area;
        assert!(
            (docs_share - 0.4).abs() < 0.04,
            "docs covers {docs_share:.3}"
        );
    }

    #[test]
    fn children_nest_inside_their_directory() {
        let index = index();
        let rects = layout(&[&index], None, &options());

        let docs = rects.iter().find(|r| r.name == "docs").unwrap();
        let a = rects.iter().find(|r| r.name == "a.pdf").unwrap();

        assert!(a.x >= docs.x && a.y >= docs.y);
        assert!(a.x + a.w <= docs.x + docs.w + 0.01);
        assert!(a.y + a.h <= docs.y + docs.h + 0.01);
        assert_eq!(a.depth, docs.depth + 1);
        assert_eq!(a.branch, docs.branch, "children inherit the folder color");
    }

    #[test]
    fn drilling_fills_the_canvas_with_the_subtree() {
        let index = index();
        let docs_id = index
            .ids()
            .find(|&id| index.name(id) == "docs")
            .unwrap()
            .get();
        let rects = layout(&[&index], Some((0, docs_id)), &options());

        let backdrop = &rects[0];
        assert_eq!(backdrop.name, "docs");
        assert_eq!(backdrop.depth, 0);
        assert!((backdrop.w - 1000.0).abs() < 0.01);

        let a = rects.iter().find(|r| r.name == "a.pdf").unwrap();
        let b = rects.iter().find(|r| r.name == "b.pdf").unwrap();
        // Inside docs, a.pdf is 75% of the bytes.
        let ratio = (a.w * a.h) / (b.w * b.h);
        assert!(
            (ratio - 3.0).abs() < 0.1,
            "a:b area ratio {ratio:.2}, wanted ~3"
        );
        assert!(
            !rects.iter().any(|r| r.name == "big.iso"),
            "outside the drill"
        );
    }

    #[test]
    fn hard_link_aliases_do_not_shape_the_map() {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let alias = RawEntry {
            fs_id: 20 | (1 << 48),
            parent_id: 5,
            name: "second-name.bin",
            size: 1000,
            allocated: 0, // the owner carries the bytes
            times: Times::default(),
            flags: EntryFlags::ALIAS,
        };
        let _ = b.push_batch(&[dir(5, 5, ""), file(20, 5, "owner.bin", 1000), alias]);
        let index = b.finish().0;

        let rects = layout(&[&index], None, &options());
        assert!(rects.iter().any(|r| r.name == "owner.bin"));
        assert!(
            !rects.iter().any(|r| r.name == "second-name.bin"),
            "zero allocated bytes → no rectangle"
        );
    }

    #[test]
    fn free_space_is_a_block_like_wiztree() {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let free = RawEntry {
            fs_id: u64::MAX,
            parent_id: 5,
            name: "Free space",
            size: 3000,
            allocated: 3000,
            times: Times::default(),
            flags: EntryFlags::SYNTHETIC,
        };
        let _ = b.push_batch(&[dir(5, 5, ""), file(20, 5, "data.bin", 1000), free]);
        let index = b.finish().0;

        let rects = layout(&[&index], None, &options());
        let free_rect = rects.iter().find(|r| r.name == "Free space").unwrap();
        assert!(free_rect.synthetic);
        // 3000 of 4000 bytes → 75% of the canvas (less the root's padding
        // and name strip).
        let share = (free_rect.w * free_rect.h) / (1000.0 * 500.0);
        assert!((share - 0.75).abs() < 0.04, "free space covers {share:.3}");
    }

    #[test]
    fn multiple_volumes_share_one_canvas_by_size() {
        let build = |label: &str, bytes: u64| {
            let caps = crate::service::scan::ntfs_caps(label.to_string());
            let mut b = IndexBuilder::new(caps, CancellationToken::new());
            let _ = b.push_batch(&[dir(5, 5, ""), file(20, 5, "data.bin", bytes)]);
            b.finish().0
        };
        let c = build("C:", 3000);
        let d = build("D:", 1000);

        let rects = layout(&[&c, &d], None, &options());
        let c_root = rects.iter().find(|r| r.name == "C:").unwrap();
        let d_root = rects.iter().find(|r| r.name == "D:").unwrap();

        let ratio = (c_root.w * c_root.h) / (d_root.w * d_root.h);
        assert!((ratio - 3.0).abs() < 0.1, "C: is 3× D:, got {ratio:.2}");
        assert_ne!(c_root.branch, d_root.branch, "volumes get distinct colors");
    }

    #[test]
    fn depth_and_minimum_size_bound_the_output() {
        let index = index();
        let shallow = layout(
            &[&index],
            None,
            &TreemapOptions {
                max_depth: 1,
                ..options()
            },
        );
        assert!(
            !shallow.iter().any(|r| r.name == "a.pdf"),
            "depth 1 stops above docs' contents"
        );

        // A tiny canvas culls everything below the root rects.
        let tiny = layout(
            &[&index],
            None,
            &TreemapOptions {
                width: 8.0,
                height: 6.0,
                max_depth: 6,
                min_area: 24.0,
                ..TreemapOptions::default()
            },
        );
        assert!(tiny.len() <= 3, "tiny canvas yields {} rects", tiny.len());
    }

    #[test]
    fn directories_reserve_a_name_strip_when_tall_enough() {
        let index = index();
        let rects = layout(&[&index], None, &options());

        let docs = rects.iter().find(|r| r.name == "docs").unwrap();
        assert!(docs.headed, "a large directory gets its name strip");
        // Children start below the strip: 1px padding + 14px header.
        let a = rects.iter().find(|r| r.name == "a.pdf").unwrap();
        assert!(
            a.y >= docs.y + 14.0,
            "a.pdf starts at {} inside docs at {} — below the header",
            a.y,
            docs.y
        );
        assert!(!a.headed, "files never reserve a strip");

        // With headers disabled, children tile right at the padding.
        let no_header = layout(
            &[&index],
            None,
            &TreemapOptions {
                dir_header_px: 0.0,
                ..options()
            },
        );
        let docs = no_header.iter().find(|r| r.name == "docs").unwrap();
        let a = no_header.iter().find(|r| r.name == "a.pdf").unwrap();
        assert!(!docs.headed);
        assert!(a.y <= docs.y + 2.5, "just the folder padding, no strip");
    }

    #[test]
    fn small_children_aggregate_instead_of_leaving_blank_space() {
        // One big file and 200 tiny ones: the tiny tail folds into a single
        // aggregate rect, and together the children tile the whole inner
        // area — the WizTree no-blank-space property.
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let _ = b.push_batch(&[dir(5, 5, ""), file(10, 5, "big.bin", 1_000_000)]);
        let names: Vec<String> = (0..200).map(|i| format!("tiny-{i}.txt")).collect();
        let tiny: Vec<RawEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(i, name)| file(100 + i as u64, 5, name, 1))
            .collect();
        let _ = b.push_batch(&tiny);
        let index = b.finish().0;

        let rects = layout(&[&index], None, &options());

        let aggregate = rects.iter().find(|r| r.aggregate).expect("tail folded");
        assert_eq!(aggregate.id, AGGREGATE_ID);
        assert_eq!(aggregate.allocated, 200);
        assert!(aggregate.name.contains("200 smaller items"));

        // Full coverage: the root's children (big + aggregate) fill its
        // inner area exactly (inner = canvas minus the 2px folder padding
        // and the 14px header strip).
        let root = rects.iter().find(|r| r.depth == 0).unwrap();
        assert!(root.expanded && root.headed);
        let inner_area = f64::from(root.w - 4.0) * f64::from(root.h - 4.0 - 14.0);
        let child_area: f64 = rects
            .iter()
            .filter(|r| r.depth == 1)
            .map(|r| f64::from(r.w) * f64::from(r.h))
            .sum();
        assert!(
            (child_area - inner_area).abs() < 2.0,
            "children cover {child_area:.1} of {inner_area:.1} px²"
        );
    }

    #[test]
    fn an_unsubdividable_directory_is_marked_unexpanded() {
        let index = index();
        // Depth 1 leaves `docs` emitted but not descended into.
        let rects = layout(
            &[&index],
            None,
            &TreemapOptions {
                max_depth: 1,
                ..options()
            },
        );
        let docs = rects.iter().find(|r| r.name == "docs").unwrap();
        assert!(!docs.expanded, "past max depth: paints as a solid block");
        assert!(!docs.headed, "no name strip without children below it");

        let full = layout(&[&index], None, &options());
        let docs = full.iter().find(|r| r.name == "docs").unwrap();
        assert!(docs.expanded && docs.headed);
    }

    #[test]
    fn the_per_directory_child_cap_folds_the_rest() {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let _ = b.push_batch(&[dir(5, 5, "")]);
        let names: Vec<String> = (0..50).map(|i| format!("f-{i}.bin")).collect();
        let files: Vec<RawEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(i, name)| file(100 + i as u64, 5, name, 10_000))
            .collect();
        let _ = b.push_batch(&files);
        let index = b.finish().0;

        let rects = layout(
            &[&index],
            None,
            &TreemapOptions {
                max_children: 10,
                ..options()
            },
        );
        let placed = rects.iter().filter(|r| !r.is_dir && !r.aggregate).count();
        let aggregate = rects.iter().find(|r| r.aggregate).expect("capped tail");
        assert_eq!(placed, 10);
        assert_eq!(aggregate.allocated, 40 * 10_000);
    }

    #[test]
    fn squarify_tiles_exactly_and_in_order() {
        let items: Vec<((), f64)> = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0]
            .iter()
            .map(|&w| ((), w))
            .collect();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 400.0,
        };
        let mut placed = Vec::new();
        squarify(&items, rect, |&(_, w), r| placed.push((w, r)));

        assert_eq!(placed.len(), items.len());
        let total_area: f64 = placed.iter().map(|(_, r)| r.area()).sum();
        assert!(
            (total_area - rect.area()).abs() < 1.0,
            "areas sum to {total_area}, canvas is {}",
            rect.area()
        );
        // Every rectangle stays inside the canvas.
        for (_, r) in &placed {
            assert!(r.x >= -0.01 && r.y >= -0.01);
            assert!(r.x + r.w <= 600.01 && r.y + r.h <= 400.01);
        }
        // The classic squarified property: aspect ratios stay tame.
        for (w, r) in &placed {
            let ratio = (r.w / r.h).max(r.h / r.w);
            assert!(ratio < 4.0, "weight {w} landed at ratio {ratio:.2}");
        }
    }
}
