<!--
  Treemap.svelte — the WizTree-style map (features.md §4.2).

  All layout happens in Rust: this component sends the canvas size, drill
  point, and depth, and paints the flat rectangle list it gets back —
  WizTree's grammar (logs/wiztree_treemap_algorithm.md): volumes are
  full-height strips; labelled folders carry a RESERVED header strip
  (`name\ (999 GiB)`) carved from the layout, framed by the two-tone 3D
  frame — 0x303030 left+bottom, 0x404040 right+top (§5.4); labelled files
  get a `name (size)` caption over their block (§5.3). Solid blocks are
  CUSHION-SHADED (§4 ridge model + the per-pixel shader recovered in
  logs/secretClaude.txt §7, incl. its ×0.8 right/bottom edge bevel) by a
  software rasterizer into one ImageData — a cost paid only when the scene
  itself changes; hover/focus recomposite the cached layer.

  Colors come from the persisted config: WizTree-style size buckets, or the
  extension category palette. Interactions: hover tooltip (clamped to the
  window — a DOM tooltip cannot leave the webview frame), click focuses and
  reveals in the folder tree, double-click drills with a short zoom,
  Backspace / Alt+Left / breadcrumb go back up.
-->
<script lang="ts">
  import { nodeInfo, nodeLineage, treemapLayout, typeBreakdown } from "../ipc";
  import { session } from "../session.svelte";
  import type { NodeInfoDto, TreemapRectDto, TypeRowDto } from "../types";
  import Icon from "../components/Icon.svelte";

  /** Folder header strip height — must match `HEADER_PX` in Rust (§5.4). */
  const HEADER_PX = 14;

  /** WizTree's 13-entry file-type palette (doc §1.1, `+0x46f0`). In ranked
   *  mode the extension with the most allocated bytes gets [0], the next
   *  [1], … ; every extension past the list takes the LAST (gray) entry.
   *  A future milestone makes the list and per-extension assignments
   *  user-configurable in settings (see `TreemapConfig` in Rust). */
  const WIZ_PALETTE = [
    "#FF8514", "#FFFF07", "#30FF45", "#A53FFF", "#FF4992", "#15A3FF",
    "#7F77FF", "#FF2DED", "#FF110F", "#81AD2E", "#15E4B6", "#BC7829",
    "#696969",
  ];

  interface Crumb {
    vol: number;
    id: number;
    name: string;
  }

  let canvas: HTMLCanvasElement | undefined = $state();
  let cssW = $state(0);
  let cssH = $state(0);

  // $state.raw: reassignment is reactive, but the 26k rect objects stay
  // plain — iterating proxied objects costs a proxy trap per property read,
  // which at scene scale is the difference between 5ms and 100ms+.
  let rects: TreemapRectDto[] = $state.raw([]);
  let crumbs: Crumb[] = $state([]);
  /** True between a drill gesture and its new layout arriving: the stale
   *  frame blurs under a loading indicator instead of being stretched. */
  let drilling = $state(false);
  let hover: TreemapRectDto | null = $state(null);
  let hoverInfo: NodeInfoDto | null = $state(null);
  let tooltipXY = $state({ x: 0, y: 0 });

  // -------------------------------------------------------------------------
  // fetching + painting
  // -------------------------------------------------------------------------

  let fetching = false;
  let refetchWanted = false;

  $effect(() => {
    void session.viewEpoch;
    void session.drill;
    void session.treemapDepth;
    void cssW;
    void cssH;
    void refetch();
  });

  /** Ranked mode's color assignment: the top extensions by allocated bytes,
   *  in rank order (row i → WIZ_PALETTE[i]). Refreshed per view epoch —
   *  NOT per resize/drill, so colors stay stable while browsing, like
   *  WizTree's. */
  let extRows: TypeRowDto[] = $state.raw([]);

  $effect(() => {
    void session.viewEpoch;
    void (async () => {
      const rows = await typeBreakdown(WIZ_PALETTE.length - 1);
      // by_extension folds the tail into one extra row past the limit;
      // keep only the real top-(N-1) buckets.
      extRows = rows.slice(0, WIZ_PALETTE.length - 1);
    })();
  });

  // Two paint layers, so hover never repays the cost of the scene:
  // the scene (all rects) renders into an offscreen canvas only when its
  // inputs change; hover/focus changes just blit it and add two outlines.
  $effect(() => {
    void session.colorMode;
    void session.typeFilter.size;
    void rects;
    void extRows;
    drawScene();
  });

  // Coalesce to one composite per animation frame: over 2px tiles the hover
  // target changes on nearly every mousemove event, and mice report faster
  // than the display paints.
  let compositeQueued = false;
  function scheduleComposite() {
    if (compositeQueued) return;
    compositeQueued = true;
    requestAnimationFrame(() => {
      compositeQueued = false;
      composite();
    });
  }

  $effect(() => {
    void session.focus;
    void hover;
    scheduleComposite();
  });

  async function refetch() {
    if (cssW < 10 || cssH < 10) return;
    if (fetching) {
      refetchWanted = true;
      return;
    }
    fetching = true;
    try {
      rects = await treemapLayout(session.drill, cssW, cssH, session.treemapDepth);
    } finally {
      fetching = false;
      if (refetchWanted) {
        refetchWanted = false;
        void refetch();
      } else {
        drilling = false; // the new frame draws instantly, un-blurred
      }
    }
  }

  function cssVar(name: string): string {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  /** Everything the per-rect color decision needs, resolved ONCE per scene:
   *  plain structures instead of proxied session state (26k rects × proxied
   *  reads is real money), and CSS variables read once, not per rect. */
  interface PaintPalette {
    mode: "ranked" | "extension";
    /** extension → palette rank, from the view-epoch type breakdown. */
    rank: Map<string, number>;
    categories: string[]; // slots 1..8
    neutral: string;
    synthetic: string;
  }

  function buildPalette(): PaintPalette {
    const rank = new Map<string, number>();
    extRows.forEach((row, i) => rank.set(row.extension, i));
    return {
      mode: session.colorMode,
      rank,
      categories: Array.from({ length: 8 }, (_, i) => cssVar(`--category-${i + 1}`)),
      neutral: cssVar("--text-muted"),
      synthetic: cssVar("--surface-hover"),
    };
  }

  const WIZ_OTHER = WIZ_PALETTE[WIZ_PALETTE.length - 1];

  function leafColor(r: TreemapRectDto, p: PaintPalette): string {
    if (r.synthetic) return p.synthetic;
    if (p.mode === "ranked") {
      // WizTree's scheme: rank i → palette[i]; unranked extensions — and
      // unexpanded folders standing in for whole subtrees — take the last
      // (gray) entry.
      if (r.is_dir) return WIZ_OTHER;
      const rank = p.rank.get(extensionOf(r.name));
      return rank === undefined ? WIZ_OTHER : WIZ_PALETTE[rank];
    }
    // "extension" mode: the category palette until per-extension color
    // configuration lands (see TreemapConfig in Rust).
    return r.category > 0 ? p.categories[r.category - 1] : p.neutral;
  }

  // -------------------------------------------------------------------------
  // cushion shading (van Wijk & van de Wetering, as WizTree implements it)
  // -------------------------------------------------------------------------
  //
  // The ridge/accumulation model is doc §4: every node adds a parabolic
  // ridge over its own rectangle to a COPY of its parent's surface
  // coefficients, with height damped by F at every descent; only the
  // depth-0 drive strip skips its own ridge (IsChild=1). (The current doc
  // words the gate as "folders only", but that literal reading leaves
  // root-level files on a flat zero surface — real WizTree, the earlier
  // decompilation pass, and van Wijk all ridge leaves too.) The per-pixel
  // shader is the one recovered in logs/secretClaude.txt §7: surface
  // normal from the coefficient derivatives, normalized, dotted with an
  // unnormalized light vector, ceiling-clamped at 1, right/bottom edge
  // bevelled ×0.8 (blocks under 5px per side skip the bevel).
  //
  // H0/F/L are the values WizTree reads from its binary; the docs pin them
  // only as "SequoiaView-lineage" reference values, used here. IA/IC/RATIO
  // are the neutral set (grayscale mode's), so brightness == intensity.
  const H0 = 0.5;
  const F = 0.75;
  const LX = 0.09;
  const LY = 0.09;
  const LZ = 1.0;
  // Lifted from the neutral set (IA=1, IC=0): IC raises the shadow floor a
  // quarter, IA keeps the top of the range just past 1.0 so highlights stay.
  const IA = 0.85;
  const IC = 0.25;
  const RATIO = 1.0;
  const EDGE = 0.8;

  type Surface = [number, number, number, number]; // s1x, s2x, s1y, s2y

  /** Add a node's cushion ridge (doc §4) to a copy of `c`. The per-axis
   *  gate is WizTree's: only when the span rounds to a nonzero width. */
  function addRidge(c: Surface, r: TreemapRectDto, h: number): Surface {
    let [s1x, s2x, s1y, s2y] = c;
    const x2 = r.x + r.w;
    const y2 = r.y + r.h;
    if (Math.round(r.w) !== 0) {
      const k = (h * 4) / r.w;
      s1x += k * (x2 + r.x);
      s2x -= k;
    }
    if (Math.round(r.h) !== 0) {
      const k = (h * 4) / r.h;
      s1y += k * (y2 + r.y);
      s2y -= k;
    }
    return [s1x, s2x, s1y, s2y];
  }

  const rgbCache = new Map<string, [number, number, number]>();

  /** CSS color → [r, g, b]. Handles #rgb/#rrggbb and rgb()/rgba() — the
   *  forms our theme variables resolve to. */
  function parseColor(css: string): [number, number, number] {
    const cached = rgbCache.get(css);
    if (cached) return cached;
    let out: [number, number, number] = [128, 128, 128];
    const s = css.trim();
    if (s.startsWith("#")) {
      const hex = s.slice(1);
      if (hex.length >= 6) {
        out = [
          parseInt(hex.slice(0, 2), 16),
          parseInt(hex.slice(2, 4), 16),
          parseInt(hex.slice(4, 6), 16),
        ];
      } else if (hex.length >= 3) {
        out = [
          parseInt(hex[0] + hex[0], 16),
          parseInt(hex[1] + hex[1], 16),
          parseInt(hex[2] + hex[2], 16),
        ];
      }
    } else {
      const m = s.match(/rgba?\(([^)]+)\)/);
      if (m) {
        const p = m[1].split(",").map((v) => parseFloat(v));
        if (p.length >= 3) out = [p[0], p[1], p[2]];
      }
    }
    rgbCache.set(css, out);
    return out;
  }

  /** The offscreen scene layer and the hit-test grid, rebuilt with it. */
  let scene: HTMLCanvasElement | null = null;
  /** Reused pixel buffer for the cushion pass (8MB at 1080p — worth reusing). */
  let sceneImg: ImageData | null = null;
  const CELL = 64;
  let grid: TreemapRectDto[][] = [];
  let gridCols = 0;
  /** O(1) focus-outline lookup instead of a 26k-element find per hover. */
  let byId: Map<string, TreemapRectDto> = new Map();

  function drawScene() {
    if (!canvas || cssW < 10 || cssH < 10) return;
    const started = performance.now();
    const dpr = window.devicePixelRatio || 1;
    const deviceW = Math.round(cssW * dpr);
    const deviceH = Math.round(cssH * dpr);
    if (canvas.width !== deviceW || canvas.height !== deviceH) {
      canvas.width = deviceW;
      canvas.height = deviceH;
    }
    scene ??= document.createElement("canvas");
    if (scene.width !== deviceW || scene.height !== deviceH) {
      scene.width = deviceW;
      scene.height = deviceH;
    }
    const ctx = scene.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    buildGrid();

    const surface = cssVar("--surface-sunken");
    const headerText = cssVar("--text-primary");
    const aggregateFill = cssVar("--border");
    const palette = buildPalette();
    // A plain Set copy: the loop must not read proxied session state per rect.
    const filter = new Set(session.typeFilter);
    const filterOn = filter.size > 0;

    ctx.font = "10px Inter, sans-serif";
    ctx.textBaseline = "top";

    // Hairlines are one DEVICE pixel, not one CSS pixel — on a 150%-scaled
    // display that is a third thinner. Thinner than this isn't physically
    // possible: the antialiaser would render a sub-pixel line as a full
    // pixel at lower opacity (fainter, not narrower). Coordinates snap to
    // the device grid so the line lands crisp on exactly one pixel row.
    const px = 1 / dpr;
    const snap = (v: number) => Math.round(v * dpr) / dpr;
    ctx.lineWidth = px;

    // ---- cushion pass -----------------------------------------------------
    // Software-rasterize every solid block into one ImageData buffer, then
    // put it down in a single call; frames/labels draw on top with vector
    // ops. Expanded folders rasterize nothing themselves — their ridge only
    // bends the surface their descendants are shaded with — so the header
    // strip and 2px margins (§5.4) show the background.
    if (
      !sceneImg ||
      sceneImg.width !== deviceW ||
      sceneImg.height !== deviceH
    ) {
      sceneImg = ctx.createImageData(deviceW, deviceH);
    }
    const data = sceneImg.data;
    const bg = parseColor(surface);
    // Background fill via a u32 view (ImageData is RGBA bytes; little-endian
    // u32 packs as ABGR).
    new Uint32Array(data.buffer).fill(
      (0xff << 24) | (bg[2] << 16) | (bg[1] << 8) | bg[0],
    );

    /** §7 of the recovered shader: per-pixel normal → light → brightness.
     *  Bounds round INWARD (trunc(lo+0.5) .. trunc(hi−0.5)) in device px. */
    const shadeBlock = (
      r: TreemapRectDto,
      s: Surface,
      rgb: [number, number, number],
      dimmed: boolean,
    ) => {
      const right = Math.trunc((r.x + r.w) * dpr - 0.5);
      const bottom = Math.trunc((r.y + r.h) * dpr - 0.5);
      const left = Math.trunc(r.x * dpr + 0.5);
      const top = Math.trunc(r.y * dpr + 0.5);
      const edge = bottom - top < 5 || right - left < 5 ? 1.0 : EDGE;
      const l = Math.max(left, 0);
      const t = Math.max(top, 0);
      const rr = Math.min(right, deviceW - 1);
      const bb = Math.min(bottom, deviceH - 1);
      const [s1x, s2x, s1y, s2y] = s;
      for (let py = t; py <= bb; py++) {
        const ny = -(2 * s2y * ((py + 0.5) / dpr) + s1y);
        const nyLight = ny * LY + LZ;
        const ny21 = ny * ny + 1;
        let at = (py * deviceW + l) * 4;
        for (let pxi = l; pxi <= rr; pxi++, at += 4) {
          const nx = -(2 * s2x * ((pxi + 0.5) / dpr) + s1x);
          let intensity = (nx * LX + nyLight) / Math.sqrt(nx * nx + ny21);
          if (intensity > 1) intensity = 1;
          if (pxi === right || py === bottom) intensity *= edge;
          let bright = (IA * intensity + IC) * RATIO;
          if (bright < 0) bright = 0;
          let cr = rgb[0] * bright;
          let cg = rgb[1] * bright;
          let cb = rgb[2] * bright;
          if (dimmed) {
            // The old 25%-alpha fill over the background, done in-buffer.
            cr = bg[0] + (cr - bg[0]) * 0.25;
            cg = bg[1] + (cg - bg[1]) * 0.25;
            cb = bg[2] + (cb - bg[2]) * 0.25;
          }
          data[at] = cr > 255 ? 255 : cr;
          data[at + 1] = cg > 255 ? 255 : cg;
          data[at + 2] = cb > 255 ? 255 : cb;
          data[at + 3] = 255;
        }
      }
    };

    // Walk the pre-order list once, re-accumulating the surface coefficients
    // WizTree threads through its recursion: a stack keyed by depth, every
    // node adding its own ridge except at depth 0 (the drive strip is
    // called with IsChild=1). Ridge height at depth d is H0·F^d.
    const stack: { depth: number; s: Surface }[] = [];
    const zero: Surface = [0, 0, 0, 0];
    for (const r of rects) {
      while (stack.length > 0 && stack[stack.length - 1].depth >= r.depth) {
        stack.pop();
      }
      const parent = stack.length > 0 ? stack[stack.length - 1].s : zero;
      if (r.is_dir && r.expanded) {
        const s =
          r.depth === 0 ? parent : addRidge(parent, r, H0 * F ** r.depth);
        stack.push({ depth: r.depth, s });
        continue;
      }
      // Solid block: add the node's OWN ridge, then shade. The current
      // doc's §4 gate reads "folders only", but taken literally it puts
      // every file without a folder ancestor below the root (a drill or
      // volume root's direct children) on a ZERO surface — normal (0,0,1),
      // intensity 1.0, a flat bright tile — which real WizTree never shows.
      // The earlier decompilation (logs/secretClaude.txt §5 step 3) and van
      // Wijk's algorithm both ridge EVERY node except the root strip, and
      // that matches WizTree's output: each block is its own pillow.
      const s = r.depth > 0 ? addRidge(parent, r, H0 * F ** r.depth) : parent;
      const dimmed =
        filterOn && !r.aggregate && !r.synthetic && !r.is_dir &&
        !filter.has(extensionOf(r.name));
      const rgb = parseColor(
        r.aggregate ? aggregateFill : leafColor(r, palette),
      );
      shadeBlock(r, s, rgb, dimmed);
    }
    ctx.putImageData(sceneImg, 0, 0);

    // Label pass, per §5.3/§5.4. Labelled folder groups get their name in
    // the RESERVED header strip (carved from the layout in Rust, so no
    // child ever collides with it) plus the two-tone 3D frame. File-style
    // labels — files, aggregates, unexpanded folders — overdraw the block.
    // Walked in REVERSE so a parent's frame lands above its descendants'
    // edges (the list is pre-order: reversed, descendants come first).
    // `headed` is Rust's full gate (both thresholds), so a drawn label
    // always includes the size string, exactly like WizTree's.
    for (let i = rects.length - 1; i >= 0; i--) {
      const r = rects[i];
      if (r.is_dir && r.expanded) {
        if (!r.headed) continue; // unlabelled group: no frame, no text (§5.4)
        // 3D frame around the whole folder cell (§5.4): 0x303030 on the
        // LEFT + BOTTOM edges, 0x404040 on the RIGHT + TOP — WizTree's
        // subtle raised look between nested folders.
        const rx = snap(r.x) + px / 2;
        const ry = snap(r.y) + px / 2;
        const rw = Math.max(0, snap(r.x + r.w) - snap(r.x) - px);
        const rh = Math.max(0, snap(r.y + r.h) - snap(r.y) - px);
        ctx.strokeStyle = "#303030";
        ctx.beginPath();
        ctx.moveTo(rx, ry);
        ctx.lineTo(rx, ry + rh);
        ctx.lineTo(rx + rw, ry + rh);
        ctx.stroke();
        ctx.strokeStyle = "#404040";
        ctx.beginPath();
        ctx.moveTo(rx + rw, ry + rh);
        ctx.lineTo(rx + rw, ry);
        ctx.lineTo(rx, ry);
        ctx.stroke();
        // Header text sits in its own strip on the surface background —
        // clipped to the strip (TextRect semantics), never wrapped.
        const label = `${r.name}\\ (${r.allocated_display})`;
        ctx.save();
        ctx.beginPath();
        ctx.rect(r.x + 2, r.y + 1, Math.max(0, r.w - 4), HEADER_PX);
        ctx.clip();
        ctx.fillStyle = headerText;
        ctx.fillText(label, r.x + 3, r.y + 2.5);
        ctx.restore();
        continue;
      }
      if (!r.headed) continue;
      // Leaf caption `name (size)`, drawn into the block inset by
      // {left+4, bottom−2} (§5.3). All-or-nothing is our web adjustment in
      // place of TextRect clipping: wrap at spaces into as many lines as
      // the box is tall; if even wrapped it cannot fit, no text at all.
      const label = `${r.is_dir ? `${r.name}\\` : r.name} (${r.allocated_display})`;
      const lines = wrapToFit(ctx, label, r.w - 8, r.h - 4);
      if (!lines) continue;
      // A halo in the surface color keeps the text readable over any block
      // color in either theme (there is no reserved strip to sit on).
      ctx.lineWidth = 2.5;
      ctx.lineJoin = "round";
      ctx.strokeStyle = surface;
      ctx.fillStyle = headerText;
      for (let li = 0; li < lines.length; li++) {
        const y = r.y + 2.5 + li * LINE_H;
        ctx.strokeText(lines[li], r.x + 4, y);
        ctx.fillText(lines[li], r.x + 4, y);
      }
      ctx.lineWidth = px;
    }

    // NOT composite() directly: a direct call would run inside the scene
    // effect's tracking scope, and composite reads `hover`/`session.focus` —
    // the whole scene would silently become hover-reactive again (the exact
    // bug a mousemove flame chart caught). The rAF callback runs untracked.
    scheduleComposite();

    // Slow-frame telemetry: lands in the app log via the console bridge, so
    // lag is diagnosable from the field without DevTools.
    const ms = performance.now() - started;
    if (ms > 16) {
      console.debug(
        `treemap: scene of ${rects.length} rects took ${ms.toFixed(1)}ms`,
      );
    }
  }

  /** Blit the scene and draw only the focus/hover outlines — the whole cost
   *  of a hover change, regardless of how many rects the scene holds. */
  function composite() {
    if (!canvas || !scene) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const started = performance.now();
    const dpr = window.devicePixelRatio || 1;

    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.drawImage(scene, 0, 0);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    const outline = (r: TreemapRectDto, color: string, width: number) => {
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.strokeRect(r.x + 1, r.y + 1, Math.max(0, r.w - 2), Math.max(0, r.h - 2));
    };
    if (session.focus) {
      const f = byId.get(`${session.focus.vol}:${session.focus.id}`);
      if (f) outline(f, cssVar("--selection"), 2);
    }
    if (hover) outline(hover, cssVar("--text-primary"), 1.5);

    const ms = performance.now() - started;
    if (ms > 8) {
      console.debug(`treemap: composite took ${ms.toFixed(1)}ms`);
    }
  }

  /** Bucket rects into a coarse grid so a mousemove hit-test walks a dozen
   *  candidates instead of every rect on a 26k-file folder. */
  function buildGrid() {
    gridCols = Math.max(1, Math.ceil(cssW / CELL));
    const gridRows = Math.max(1, Math.ceil(cssH / CELL));
    grid = Array.from({ length: gridCols * gridRows }, () => []);
    byId = new Map();
    for (const r of rects) {
      if (!r.aggregate) byId.set(`${r.vol}:${r.id}`, r);
      const c0 = Math.max(0, Math.floor(r.x / CELL));
      const c1 = Math.min(gridCols - 1, Math.floor((r.x + r.w) / CELL));
      const r0 = Math.max(0, Math.floor(r.y / CELL));
      const r1 = Math.min(gridRows - 1, Math.floor((r.y + r.h) / CELL));
      for (let row = r0; row <= r1; row++) {
        for (let col = c0; col <= c1; col++) {
          grid[row * gridCols + col].push(r);
        }
      }
    }
  }

  /** Label line height at the 10px scene font. */
  const LINE_H = 12;

  /** Break `text` at spaces into lines no wider than `availW`, at most as
   *  many lines as fit in `availH`. Returns null when the text cannot fit
   *  even wrapped — a word alone overflows the width, or the wrapped text
   *  needs more lines than the box is tall — in which case no text is
   *  drawn at all (no clipped fragments). */
  function wrapToFit(
    ctx: CanvasRenderingContext2D,
    text: string,
    availW: number,
    availH: number,
  ): string[] | null {
    const maxLines = Math.floor(availH / LINE_H);
    if (maxLines < 1) return null;
    // Cheap reject before any measureText: no glyph in 10px Inter is
    // narrower than ~4px, so a label longer than that bound can never fit.
    if (text.length * 4 > availW * maxLines) return null;

    if (ctx.measureText(text).width <= availW) return [text];
    if (maxLines === 1) return null;

    const words = text.split(" ");
    if (words.length === 1) return null; // nothing to wrap at
    const lines: string[] = [];
    let line = "";
    for (const word of words) {
      const candidate = line === "" ? word : `${line} ${word}`;
      if (ctx.measureText(candidate).width <= availW) {
        line = candidate;
        continue;
      }
      if (line === "") return null; // a single word overflows the width
      lines.push(line);
      if (lines.length === maxLines) return null;
      if (ctx.measureText(word).width > availW) return null;
      line = word;
    }
    lines.push(line);
    return lines.length <= maxLines ? lines : null;
  }

  function extensionOf(name: string): string {
    const at = name.lastIndexOf(".");
    return at > 0 ? name.slice(at + 1).toLowerCase() : "";
  }

  /** Legend entries for the active color mode. */
  const legend = $derived.by(() => {
    if (session.colorMode === "ranked") {
      // The ranked extensions in palette order, then the catch-all gray.
      const entries = extRows.map((row, i) => ({
        color: WIZ_PALETTE[i],
        label: row.extension === "" ? "no ext" : row.extension,
      }));
      entries.push({ color: WIZ_OTHER, label: "other" });
      return entries;
    }
    const kinds = [
      ["Folders", 1],
      ["Executables", 2],
      ["Archives", 3],
      ["Images", 4],
      ["Video", 5],
      ["Audio", 6],
      ["Documents", 7],
      ["Code", 8],
    ] as const;
    return kinds.map(([label, slot]) => ({ color: `var(--category-${slot})`, label }));
  });

  // -------------------------------------------------------------------------
  // hit testing + interactions
  // -------------------------------------------------------------------------

  /** Topmost = deepest rect under the cursor (children paint over parents).
   *  Looks only at the cursor's grid cell, not the whole rect list. */
  function hitTest(x: number, y: number): TreemapRectDto | null {
    const cell =
      grid[Math.floor(y / CELL) * gridCols + Math.floor(x / CELL)];
    if (!cell) return null;
    let best: TreemapRectDto | null = null;
    for (const r of cell) {
      if (x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h) {
        if (!best || r.depth > best.depth) best = r;
      }
    }
    return best;
  }

  let infoTimer: ReturnType<typeof setTimeout> | undefined;

  function onMouseMove(e: MouseEvent) {
    const bounds = canvas?.getBoundingClientRect();
    if (!bounds) return;
    const x = e.clientX - bounds.left;
    const y = e.clientY - bounds.top;
    tooltipXY = { x, y };

    const hit = hitTest(x, y);
    if (hit?.id !== hover?.id || hit?.vol !== hover?.vol) {
      hover = hit;
      hoverInfo = null;
      clearTimeout(infoTimer);
      // Aggregates carry no real node id; their tooltip is self-contained.
      if (hit && !hit.aggregate) {
        infoTimer = setTimeout(async () => {
          hoverInfo = await nodeInfo(hit.vol, hit.id);
        }, 120);
      }
    }
  }

  /** Keep the tooltip inside the canvas, flipping sides near the edges.
   *  (It cannot leave the window: a DOM element is clipped to the webview;
   *  escaping the frame would need a separate borderless OS window.) */
  const tooltipPos = $derived.by(() => {
    const W = 270;
    const H = 66;
    let x = tooltipXY.x + 14;
    let y = tooltipXY.y + 14;
    if (x + W > cssW) x = Math.max(4, tooltipXY.x - W - 10);
    if (y + H > cssH) y = Math.max(4, tooltipXY.y - H - 10);
    return { x, y };
  });

  function onClick(e: MouseEvent) {
    const bounds = canvas?.getBoundingClientRect();
    if (!bounds) return;
    const hit = hitTest(e.clientX - bounds.left, e.clientY - bounds.top);
    if (hit && !hit.aggregate) {
      session.focus = { vol: hit.vol, id: hit.id };
      session.revealEpoch += 1; // ask the folder tree to expand to it
    }
  }

  function onDblClick(e: MouseEvent) {
    const bounds = canvas?.getBoundingClientRect();
    if (!bounds) return;
    const hit = hitTest(e.clientX - bounds.left, e.clientY - bounds.top);
    if (hit?.is_dir && !hit.synthetic && !hit.aggregate) {
      void drillTo({ vol: hit.vol, id: hit.id });
    }
  }

  /** Drill: blur the stale frame under a loading indicator; the new layout
   *  replaces it in one paint when it arrives (no stretch, no zoom). */
  async function drillTo(target: { vol: number; id: number } | null) {
    drilling = true;
    hover = null;
    session.drill = target;
    if (target) {
      await rebuildCrumbs(target);
    } else {
      crumbs = [];
    }
  }

  async function rebuildCrumbs(target: { vol: number; id: number }) {
    const chain = await nodeLineage(target.vol, target.id);
    const info = await nodeInfo(target.vol, target.id);
    const names = info ? info.path.split(/[\\/]/).filter((s) => s !== "") : [];
    // lineage ids and path segments align root..target.
    crumbs = chain.map((id, i) => ({
      vol: target.vol,
      id,
      name: names[i] ?? "…",
    }));
  }

  function crumbClick(i: number) {
    const crumb = crumbs[i];
    crumbs = crumbs.slice(0, i + 1);
    drilling = true;
    hover = null;
    session.drill = { vol: crumb.vol, id: crumb.id };
  }

  function drillUp() {
    if (crumbs.length > 1) {
      crumbClick(crumbs.length - 2);
    } else {
      void drillTo(null);
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Backspace" || (e.altKey && e.key === "ArrowLeft")) {
      e.preventDefault();
      drillUp();
    }
  }

  // Drill changes made elsewhere still need matching crumbs.
  $effect(() => {
    const drill = session.drill;
    if (drill && !crumbs.some((c) => c.vol === drill.vol && c.id === drill.id)) {
      void rebuildCrumbs(drill);
    }
    if (!drill) crumbs = [];
  });
</script>

<div class="treemap">
  <div class="crumbs">
    <button class="crumb" onclick={() => void drillTo(null)}>
      All volumes
    </button>
    {#each crumbs as crumb, i (crumb.id)}
      <span class="sep">›</span>
      <button class="crumb" onclick={() => crumbClick(i)}>{crumb.name}</button>
    {/each}
    <span class="flex"></span>
    <div class="legend">
      {#each legend as item (item.label)}
        <span class="swatch" style:background={item.color}></span>
        <span class="legend-label">{item.label}</span>
      {/each}
    </div>
  </div>

  <div
    class="canvas-wrap"
    bind:clientWidth={cssW}
    bind:clientHeight={cssH}
    role="img"
    aria-label="Treemap of disk usage"
  >
    <canvas
      bind:this={canvas}
      class:stale={drilling}
      style:width="{cssW}px"
      style:height="{cssH}px"
      tabindex="0"
      onmousemove={onMouseMove}
      onmouseleave={() => (hover = null)}
      onclick={onClick}
      ondblclick={onDblClick}
      onkeydown={onKeydown}
    ></canvas>

    {#if drilling}
      <div class="loading" aria-label="Loading">
        <span class="spinner"><Icon name="arrow-repeat" size={28} /></span>
      </div>
    {/if}

    {#if hover}
      <div class="tooltip" style:left="{tooltipPos.x}px" style:top="{tooltipPos.y}px">
        <div class="tip-name">{hover.name}</div>
        <div class="tip-detail">
          {hover.allocated_display} on disk · {hover.size_display}
        </div>
        {#if hoverInfo}
          <div class="tip-path">{hoverInfo.path}</div>
        {/if}
      </div>
    {/if}

    {#if rects.length === 0}
      <div class="empty">Scan a drive to draw the map.</div>
    {/if}
  </div>
</div>

<style>
  .treemap {
    position: relative;
    display: flex;
    flex-direction: column;
    min-height: 0;
    /* min-width matters: flex items default to min-width auto, and the
     * canvas's explicit pixel width would otherwise stop the whole column
     * from ever shrinking when the window narrows. */
    min-width: 0;
    flex: 1;
    gap: var(--space-1);
  }

  .crumbs {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    min-height: 24px;
    overflow: hidden;
  }
  .crumb {
    border: none;
    background: none;
    padding: var(--space-1);
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-caption);
    cursor: pointer;
    white-space: nowrap;
  }
  .crumb:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
    border-radius: var(--radius-small);
  }
  .sep {
    color: var(--text-muted);
  }
  .flex {
    flex: 1;
  }

  .legend {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    overflow: hidden;
    white-space: nowrap;
  }
  .swatch {
    width: 10px;
    height: 10px;
    border-radius: 2px;
    flex: 0 0 auto;
  }
  .legend-label {
    margin-right: var(--space-2);
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }

  .canvas-wrap {
    position: relative;
    flex: 1;
    min-height: 0;
    min-width: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    overflow: hidden;
    background: var(--surface-sunken);
  }
  canvas {
    display: block;
    outline: none;
  }
  /* Drill transition: the stale frame blurs in place — never stretched —
   * until the new layout paints over it. */
  canvas.stale {
    filter: blur(5px);
  }

  .loading {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--text-secondary);
    pointer-events: none;
  }
  .spinner {
    display: grid;
    place-items: center;
    animation: treemap-spin 0.9s linear infinite;
  }
  @keyframes treemap-spin {
    to {
      transform: rotate(360deg);
    }
  }

  .tooltip {
    position: absolute;
    z-index: 10;
    max-width: 270px;
    padding: var(--space-2);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    pointer-events: none;
    box-shadow: 0 4px 12px var(--overlay);
  }
  .tip-name {
    color: var(--text-primary);
    font-size: var(--font-size-body);
    font-weight: var(--font-weight-semibold);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .tip-detail {
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
  }
  .tip-path {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
    word-break: break-all;
  }

  .empty {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--text-muted);
    pointer-events: none;
  }
</style>
