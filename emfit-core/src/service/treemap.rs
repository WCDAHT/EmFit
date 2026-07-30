//! Squarified treemap layout, computed in Rust (features.md §9).
//!
//! The webview never lays out rectangles: it sends a canvas size and a drill
//! point, and gets back a **flat list of rectangles** to paint. Laying out
//! hundreds of thousands of nested rectangles in JS is not viable; painting a
//! precomputed list on a canvas is.
//!
//! # Layout, WizTree-style
//!
//! The geometry follows WizTree's recovered pipeline
//! (logs/wiztree_treemap_algorithm.md), minus the cushion shading:
//!
//! - **Root children are full-height vertical strips** (§3.8), each width
//!   rounded independently from its own share, swept left to right with a
//!   1px gap. This level is a plain proportional split, *not* squarified —
//!   the 2-D squarify starts one level down. A drill is WizTree's zoom mode
//!   (§3.11): the focused node fills the whole frame.
//! - **Labelled folders carve a header strip** off the top *before* their
//!   children are partitioned (§5.4): the child area shrinks to
//!   `{x+2, y+(minLabelH+1), w−4, h−(minLabelH+2)}`, so no child ever
//!   overlaps the label. Unlabelled folders hand children their entire
//!   rectangle — no header gap, no frame. Labels gate on BOTH thresholds
//!   ([`LABEL_MIN_H`], [`LABEL_MIN_W`]) and always carry the size string;
//!   file labels overdraw their block and have no layout effect (§5.3).
//! - **The squarify partition is §6 verbatim**: rows grow while the worst
//!   aspect ratio improves (degenerate ratios zeroed via a `1e-5` epsilon,
//!   ±INF collapsed to 0), thickness and totals recompute per strip from the
//!   leftover rectangle, the strip advance is floored at 0.25px, a leftover
//!   under 0.25px on its short side is handed whole to every remaining
//!   child, and past 1000 strips (or sibling 49999) items get zero rects.
//! - **Child rects snap to integer pixel edges before recursing** (§5.4), so
//!   siblings share the rounded boundary and the tiling stays exact.
//! - **Recursion stops below 1px** in either dimension (§5.1).
//!
//! # Geometry is allocated bytes, always
//!
//! Rectangles are sized by **allocated** size, not logical size — confirmed
//! against WizTree (features.md §4.2). A folder holding one 100 MB file under
//! four hard-link names draws one 100 MB rectangle, not four: hard-link
//! aliases carry zero allocated bytes and vanish from the geometry, sparse
//! and compressed files occupy their real footprint, and the synthetic
//! free-space row — hidden by default behind
//! [`TreemapOptions::show_free_space`] — fills the volume out to its
//! capacity when enabled. Labels may still show logical size; only the
//! geometry is allocated.
//!
//! # Web adjustments
//!
//! WizTree simply leaves sub-pixel rectangles undrawn; over IPC we cannot
//! afford blank space or unbounded payloads, so three knobs remain ours:
//! recursion also stops at [`TreemapOptions::max_depth`], children below
//! [`TreemapOptions::min_area`] (or past [`TreemapOptions::max_children`])
//! fold into one aggregate rectangle per directory, and the output stays
//! bounded by *pixels*, not by index size — a 5M-file volume and a 5K-file
//! volume produce similarly sized lists (roadmap M3, 100 ms budget).

use crate::model::index::{Index, NodeId};
use crate::service::filetype::FileKind;
use crate::service::view::human_size;

/// The `id` carried by an aggregate ("N smaller items") rectangle — not a
/// real node; actions must ignore it.
pub const AGGREGATE_ID: u32 = u32::MAX;

/// WizTree's recursion guard: a rectangle under 1px in either dimension is
/// never subdivided.
const MIN_SIDE: f64 = 1.0;

/// Label thresholds (§3.7: `MulDiv(12|80, DPI, 96)` — the webview is always
/// logical-96-dpi pixels). Both gate the WHOLE label: a folder is labelled
/// when `h > 12 && w >= 80` (§5.4), a leaf when `h > 12 && w > 80` (§5.3 —
/// strict on both). A drawn label always carries the size string.
const LABEL_MIN_H: f64 = 12.0;
const LABEL_MIN_W: f64 = 80.0;

/// Folder header strip height (`minLabelHeight` + breathing room in §5.4's
/// carve formula). The frontend's `HEADER_PX` must match.
const HEADER_PX: f64 = 14.0;

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
    /// Labelled: the rect clears BOTH label thresholds ([`LABEL_MIN_H`],
    /// [`LABEL_MIN_W`] — §5.3/§5.4). For an `expanded` directory this means
    /// a [`HEADER_PX`] header strip was carved off the top and children tile
    /// **below** it; for everything else (files, aggregates, unexpanded
    /// directories) the label overdraws the block and has no layout effect.
    pub headed: bool,
    /// This directory's children were laid out inside it — tiling its
    /// rectangle (below the header strip when `headed`), so its own fill
    /// never shows. False for a directory too small (or too deep) to
    /// subdivide — paint those as solid blocks, since nothing tiles them.
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
    /// Draw the synthetic free-space row (and any other builder-invented
    /// *file* rows). Off by default — a settings toggle; synthetic
    /// *directories* (orphan placeholders holding real files) always show.
    pub show_free_space: bool,
}

impl Default for TreemapOptions {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 640.0,
            max_depth: 6,
            min_area: 2.0,
            max_children: 2000,
            show_free_space: false,
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

    /// Snap to integer pixel edges the way WizTree rounds each child rect
    /// before recursing: `Round(x), Round(y), Round(x+w), Round(y+h)`.
    /// Adjacent cells share the exact float boundary, so they share the
    /// rounded one too — the tiling stays gapless.
    fn rounded(&self) -> Rect {
        let x0 = self.x.round();
        let y0 = self.y.round();
        Rect {
            x: x0,
            y: y0,
            w: (self.x + self.w).round() - x0,
            h: (self.y + self.h).round() - y0,
        }
    }
}

/// Lay out the treemap for the current drill level.
///
/// `drill: None` slices every scanned volume into a full-height column ∝ its
/// size, WizTree's root strip (features.md §4.2 multi-volume). `Some((vol,
/// id))` is zoom mode: that node's subtree fills the whole canvas.
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
        w: f64::from(options.width).round(),
        h: f64::from(options.height).round(),
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
            // WizTree's root strip: every volume is a full-height vertical
            // column, width ∝ what it holds, swept left to right. Column
            // edges come from the cumulative share (no drift), rounded to
            // integers, with a 1px gap between columns.
            let items: Vec<(usize, f64)> = indices
                .iter()
                .enumerate()
                .map(|(slot, index)| {
                    let root = index.node(index.root());
                    let mut weight = root.total_allocated() as f64;
                    // With free space hidden, a strip's width reflects only
                    // what its children will actually tile — otherwise a
                    // near-empty large drive gets a huge, mostly-meaningless
                    // column.
                    if !options.show_free_space {
                        for &child in index.children(index.root()) {
                            let node = index.node(child);
                            if node.is_synthetic() && !node.is_directory() {
                                weight -= node.allocated() as f64;
                            }
                        }
                    }
                    (slot, weight)
                })
                .filter(|&(_, weight)| weight > 0.0)
                .collect();
            let total: f64 = items.iter().map(|(_, w)| w).sum();
            if total <= 0.0 {
                return out;
            }

            // §3.8: each strip's width rounds independently from its own
            // share (`rightEdge = leftCursor + round(totalWidth·share)`), so
            // the last strip may over/undershoot the frame edge by a pixel —
            // WizTree accepts that, and the canvas clips it.
            let mut x = canvas.x;
            for &(slot, weight) in &items {
                let strip_w = (canvas.w * (weight / total)).round();
                let column = Rect {
                    x,
                    y: canvas.y,
                    w: strip_w,
                    h: canvas.h,
                };
                x += strip_w + 1.0;

                let index = indices[slot];
                let vol = slot as u16;
                let at = emit(&mut out, vol, index, index.root(), column, 0, slot as u16);
                maybe_descend(
                    index,
                    vol,
                    index.root(),
                    at,
                    column,
                    1,
                    Some(slot as u16),
                    options,
                    &mut out,
                );
            }
        }
    }
    out
}

/// Subdivide an emitted directory rect when depth and size allow, marking
/// `expanded` on it accordingly. A directory left unexpanded paints as a
/// solid block, so no space is ever blank.
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
    // A labelled folder carves its header strip off the top BEFORE the
    // partition (§5.4), so no child can ever occupy the label:
    //   origin → {x + 2, y + (minLabelH + 1)},  size → {w − 4, h − (minLabelH + 2)}
    let inner = if out[at].headed {
        Rect {
            x: rect.x + 2.0,
            y: rect.y + (HEADER_PX + 1.0),
            w: rect.w - 4.0,
            h: rect.h - (HEADER_PX + 2.0),
        }
    } else {
        rect
    };
    // WizTree's guard: nothing under 1px on a side is subdivided. max_depth
    // is our web addition (bounds the payload).
    if child_depth > options.max_depth || inner.w < MIN_SIDE || inner.h < MIN_SIDE {
        return; // stays unexpanded: painted solid by the frontend
    }
    if descend(index, vol, id, inner, child_depth, branch, options, out) {
        out[at].expanded = true;
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

/// Lay out `dir`'s children into `rect` — the directory's **entire**
/// rectangle, WizTree-style — tiling it completely: children below
/// [`TreemapOptions::min_area`] (or past [`TreemapOptions::max_children`])
/// fold into one aggregate rectangle instead of being dropped, so the map
/// never shows blank space. Returns false when there was nothing to place.
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
) -> bool {
    // Children with real footprint, largest first — squarify requires
    // descending weights. Hard-link aliases (allocated 0) drop out here,
    // which is exactly the one-owner accounting the geometry needs.
    let mut children: Vec<(NodeId, f64)> = index
        .children(dir)
        .iter()
        .filter(|&&child| {
            // Synthetic file rows (the free-space block) hide behind the
            // settings toggle; synthetic directories are orphan placeholders
            // holding real files and always show.
            let node = index.node(child);
            options.show_free_space || !node.is_synthetic() || node.is_directory()
        })
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
        return false;
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

    squarify(&tiles, rect, |(tile, _), cell| {
        // WizTree rounds each child rect to integer pixel edges *before*
        // recursing; everything below tiles the rounded rect.
        let cell = cell.rounded();
        // Depth-1 children of the drill root define the folder-color groups;
        // everything deeper inherits.
        let branch = inherited_branch.unwrap_or(out.len() as u16);
        match tile {
            Tile::Node(child) => {
                let at = emit(out, vol, index, *child, cell, depth, branch);
                maybe_descend(
                    index,
                    vol,
                    *child,
                    at,
                    cell,
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
                x: cell.x as f32,
                y: cell.y as f32,
                w: cell.w as f32,
                h: cell.h as f32,
                depth,
                is_dir: false,
                synthetic: false,
                category: 0,
                branch,
                headed: cell.h > LABEL_MIN_H && cell.w > LABEL_MIN_W,
                expanded: false,
                aggregate: true,
                name: format!("{count} smaller items ({})", human_size(*allocated)),
                size: *size,
                allocated: *allocated,
            }),
        }
    });
    true
}

/// Push one rectangle; returns its position so the caller can mark
/// `expanded`.
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
        // §5.4 folders use `>=` on the width; §5.3 leaves are strict on both.
        headed: if node.is_directory() {
            rect.h > LABEL_MIN_H && rect.w >= LABEL_MIN_W
        } else {
            rect.h > LABEL_MIN_H && rect.w > LABEL_MIN_W
        },
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

/// Degenerate-size epsilon in the aspect-ratio comparison (§6.3): ratios of
/// near-zero extents are zeroed so they can't blow up the row comparison.
const RATIO_EPSILON: f64 = 1e-5;

/// Floor for the strip advance and the leftover cutoff (§6.5).
const STRIP_FLOOR: f64 = 0.25;

/// §6.1 overflow guards: at most 1000 subdivision strips, and siblings past
/// index 49999 get a zero rect (invisible). The recursion enters at depth 1
/// (§5.4 passes `param2 = 1`).
const STRIP_DEPTH_CAP: usize = 1000;
const SIBLING_CAP: usize = 49_999;

/// The squarified treemap partition, following the decompiled
/// `SquarifyLayout` (§6) — the classic Bruls/Huizing/van Wijk algorithm.
/// WizTree forces the tree into size-descending order before rendering
/// (§3.5); callers here must pass items sorted by descending weight.
///
/// Each strip recomputes the remaining total and works from the leftover
/// rectangle's own dimensions; the leftover origin advances by
/// `max(thickness, 0.25px)` while the extent shrinks by the true thickness;
/// once the leftover's short side is ≤ 0.25px every remaining item is handed
/// the leftover rect wholesale; and past the §6.1 depth/sibling caps items
/// get a zeroed rect and vanish — all per the decompilation.
fn squarify<T>(items: &[(T, f64)], rect: Rect, mut place: impl FnMut(&(T, f64), Rect)) {
    let mut remaining = rect;
    let mut i = 0;
    let mut depth = 1usize;

    while i < items.len() {
        // §6.1: overflow ⇒ every remaining item gets a zero rect.
        if depth >= STRIP_DEPTH_CAP || i > SIBLING_CAP {
            let zero = Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            };
            for item in &items[i..] {
                place(item, zero);
            }
            return;
        }
        // §6.2: total of the not-yet-placed children.
        let total: f64 = items[i..].iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            return;
        }
        // §6.2: split along the rect's shorter side. `w < h` → a horizontal
        // row across the full width, anchored at the top; otherwise a
        // vertical column down the full height, at the left.
        let row = remaining.w < remaining.h;
        let perp = if row { remaining.h } else { remaining.w };
        let span = if row { remaining.w } else { remaining.h };

        // §6.3: grow the row greedily while the worst aspect ratio improves;
        // close it right before the item that makes it worse.
        let mut prev_worst: Option<f64> = None;
        let mut count = 0;
        while i + count < items.len() {
            let candidate = &items[i..=i + count];
            let row_sum: f64 = candidate.iter().map(|(_, w)| w).sum();
            let thickness = (row_sum / total) * perp;
            let mut worst = 1.0f64; // 1.0 = a perfect square
            for (_, weight) in candidate {
                let extent = if row_sum > 0.0 {
                    weight * (span / row_sum)
                } else {
                    0.0
                };
                let mut ratio = if extent <= thickness || thickness <= RATIO_EPSILON {
                    if extent <= RATIO_EPSILON {
                        0.0
                    } else {
                        thickness / extent
                    }
                } else {
                    extent / thickness
                };
                if !ratio.is_finite() {
                    ratio = 0.0; // §6.3: ±INF aspect collapses to 0
                }
                worst = worst.max(ratio);
            }
            if prev_worst.is_some_and(|prev| worst > prev) {
                break;
            }
            prev_worst = Some(worst);
            count += 1;
        }

        // §6.4: lay the finalized row/column.
        let placed = &items[i..i + count];
        let row_sum: f64 = placed.iter().map(|(_, w)| w).sum();
        let thickness = (row_sum / total) * perp;
        let k = if row_sum > 0.0 { span / row_sum } else { 0.0 };
        let mut pos = if row { remaining.x } else { remaining.y };
        for item in placed {
            let extent = item.1 * k;
            let cell = if row {
                Rect {
                    x: pos,
                    y: remaining.y,
                    w: extent,
                    h: thickness,
                }
            } else {
                Rect {
                    x: remaining.x,
                    y: pos,
                    w: thickness,
                    h: extent,
                }
            };
            place(item, cell);
            pos += extent;
        }
        i += count;
        depth += 1;

        // §6.5: advance into the leftover. The origin moves by the floored
        // thickness while the extent shrinks by the true one.
        let advance = thickness.max(STRIP_FLOOR);
        if row {
            remaining.y += advance;
            remaining.h -= thickness;
        } else {
            remaining.x += advance;
            remaining.w -= thickness;
        }
        if i < items.len() && remaining.shorter() <= STRIP_FLOOR {
            // Degenerate leftover: every remaining child gets the whole of it.
            for item in &items[i..] {
                place(item, remaining);
            }
            return;
        }
    }
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

        // big.iso is 1200 of 2000 total → 60% of the canvas. The labelled
        // root carves its header strip and side margins off the child area
        // (§5.4), which costs a few percent on a 500px canvas.
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
    fn rects_snap_to_integer_pixel_edges() {
        let index = index();
        let rects = layout(&[&index], None, &options());
        // WizTree rounds every child rect before recursing; everything below
        // the root columns is integer-aligned.
        for r in &rects {
            assert_eq!(r.x, r.x.round(), "{} x={} not integer", r.name, r.x);
            assert_eq!(r.y, r.y.round(), "{} y={} not integer", r.name, r.y);
            assert_eq!(r.w, r.w.round(), "{} w={} not integer", r.name, r.w);
            assert_eq!(r.h, r.h.round(), "{} h={} not integer", r.name, r.h);
        }
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

        // Hidden by default (a settings toggle): the map shows only real
        // files, and data.bin tiles the whole child area alone.
        let rects = layout(&[&index], None, &options());
        assert!(
            !rects.iter().any(|r| r.name == "Free space"),
            "free space is hidden by default"
        );
        let data = rects.iter().find(|r| r.name == "data.bin").unwrap();
        let share = (data.w * data.h) / (1000.0 * 500.0);
        assert!((share - 1.0).abs() < 0.05, "data.bin covers {share:.3}");

        // Toggled on, it draws as a plain block like WizTree's.
        let rects = layout(
            &[&index],
            None,
            &TreemapOptions {
                show_free_space: true,
                ..options()
            },
        );
        let free_rect = rects.iter().find(|r| r.name == "Free space").unwrap();
        assert!(free_rect.synthetic);
        // 3000 of 4000 bytes → 75% of the canvas (less the root's header
        // strip and margins).
        let share = (free_rect.w * free_rect.h) / (1000.0 * 500.0);
        assert!((share - 0.75).abs() < 0.04, "free space covers {share:.3}");
    }

    #[test]
    fn volumes_slice_into_full_height_columns() {
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

        // WizTree's root strips (§3.8): full-height vertical columns, each
        // width rounded independently from its own share, 1px gap between
        // neighbors (the last strip may overshoot the frame; the canvas
        // clips it).
        assert!((c_root.h - 500.0).abs() < 0.01, "C: column is full height");
        assert!((d_root.h - 500.0).abs() < 0.01, "D: column is full height");
        assert!(
            (c_root.w - 750.0).abs() < 0.01,
            "C: holds 75% of the bytes → round(1000·0.75) = 750px, got {}",
            c_root.w
        );
        assert!(
            (d_root.w - 250.0).abs() < 0.01,
            "D: gets round(1000·0.25) = 250px, got {}",
            d_root.w
        );
        assert!(
            (d_root.x - (c_root.x + c_root.w + 1.0)).abs() < 0.01,
            "columns are separated by the 1px gap"
        );
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
    fn labelled_folders_carve_a_header_strip_before_partitioning() {
        let index = index();
        let rects = layout(&[&index], None, &options());

        let docs = rects.iter().find(|r| r.name == "docs").unwrap();
        assert!(docs.expanded && docs.headed);

        // §5.4: the child area shrinks to {x+2, y+(minLabelH+1), w−4,
        // h−(minLabelH+2)} BEFORE squarify, so no child can occupy the strip.
        let children: Vec<_> = rects
            .iter()
            .filter(|r| r.depth == docs.depth + 1)
            .collect();
        let top = children.iter().map(|r| r.y).fold(f32::MAX, f32::min);
        let left = children.iter().map(|r| r.x).fold(f32::MAX, f32::min);
        assert!(
            (top - (docs.y + 15.0)).abs() < 0.01,
            "children start below the header strip (labelH + 1)"
        );
        assert!(
            (left - (docs.x + 2.0)).abs() < 0.01,
            "children start inside the 2px side margin"
        );
        let child_area: f64 = children
            .iter()
            .map(|r| f64::from(r.w) * f64::from(r.h))
            .sum();
        let inner_area = f64::from(docs.w - 4.0) * f64::from(docs.h - 16.0);
        assert!(
            (child_area - inner_area).abs() < 2.0,
            "children cover {child_area:.1} of the carved {inner_area:.1} px²"
        );

        // Leaves are labelled too when they clear BOTH thresholds (§5.3) —
        // but the label overdraws the block; it never affects layout.
        let big = rects.iter().find(|r| r.name == "big.iso").unwrap();
        assert!(big.headed && !big.expanded);
        // b.pdf is 200 of 2000 bytes → far under the 80px width gate.
        let b = rects.iter().find(|r| r.name == "b.pdf").unwrap();
        assert!(
            b.headed == (b.h > 12.0 && b.w > 80.0),
            "leaf labels need BOTH h > 12 and w > 80"
        );
    }

    #[test]
    fn small_children_aggregate_instead_of_leaving_blank_space() {
        // One big file and 200 tiny ones: the tiny tail folds into a single
        // aggregate rect, and together the children tile the whole root
        // column — the WizTree no-blank-space property.
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
        // carved child area (the rect minus the §5.4 header strip + margins).
        let root = rects.iter().find(|r| r.depth == 0).unwrap();
        assert!(root.expanded && root.headed);
        let inner_area = f64::from(root.w - 4.0) * f64::from(root.h - 16.0);
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

        let full = layout(&[&index], None, &options());
        let docs = full.iter().find(|r| r.name == "docs").unwrap();
        assert!(docs.expanded);
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
