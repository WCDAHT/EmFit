<!--
  Treemap.svelte — the WizTree-style map (features.md §4.2).

  All layout happens in Rust: this component sends the canvas size, drill
  point, and depth, and paints the flat rectangle list it gets back —
  WizTree's visual grammar: files are plain 1px-bordered boxes with no
  labels, tiled edge to edge; directories keep a 1px padding and, when tall
  enough, a reserved name strip at the top reading `name\ (999 GiB)` (the
  strip is part of the Rust layout, so children genuinely tile below it).

  Colors come from the persisted config: WizTree-style size buckets, or the
  extension category palette. Interactions: hover tooltip (clamped to the
  window — a DOM tooltip cannot leave the webview frame), click focuses and
  reveals in the folder tree, double-click drills with a short zoom,
  Backspace / Alt+Left / breadcrumb go back up.
-->
<script lang="ts">
  import { nodeInfo, nodeLineage, treemapLayout } from "../ipc";
  import { session } from "../session.svelte";
  import type { NodeInfoDto, TreemapRectDto } from "../types";
  import Icon from "../components/Icon.svelte";

  /** Must match `TreemapOptions::dir_header_px` in Rust. */
  const HEADER_PX = 14;

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

  // Two paint layers, so hover never repays the cost of the scene:
  // the scene (all rects) renders into an offscreen canvas only when its
  // inputs change; hover/focus changes just blit it and add two outlines.
  $effect(() => {
    void session.colorMode;
    void session.sizeRanges;
    void session.typeFilter.size;
    void rects;
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
   *  plain arrays instead of proxied session state (26k rects × proxied
   *  reads is real money), and CSS variables read once, not per rect. */
  interface PaintPalette {
    mode: "size" | "extension";
    maxes: number[];
    bucketColors: string[];
    categories: string[]; // slots 1..8
    neutral: string;
    synthetic: string;
  }

  function buildPalette(): PaintPalette {
    return {
      mode: session.colorMode,
      maxes: session.sizeRanges.map((r) => r.max_bytes),
      bucketColors: session.sizeRanges.map((r) => r.color),
      categories: Array.from({ length: 8 }, (_, i) => cssVar(`--category-${i + 1}`)),
      neutral: cssVar("--text-muted"),
      synthetic: cssVar("--surface-hover"),
    };
  }

  function leafColor(r: TreemapRectDto, p: PaintPalette): string {
    if (r.synthetic) return p.synthetic;
    if (p.mode === "size") {
      if (p.maxes.length === 0) return p.categories[0];
      for (let i = 0; i < p.maxes.length; i++) {
        if (r.allocated <= p.maxes[i]) return p.bucketColors[i];
      }
      return p.bucketColors[p.bucketColors.length - 1];
    }
    return r.category > 0 ? p.categories[r.category - 1] : p.neutral;
  }

  /** The offscreen scene layer and the hit-test grid, rebuilt with it. */
  let scene: HTMLCanvasElement | null = null;
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

    ctx.fillStyle = surface;
    ctx.fillRect(0, 0, cssW, cssH);
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

    // Border discipline (WizTree): solid boxes stroke only their TOP and
    // LEFT edges, so two neighbors share a single hairline instead of
    // doubling up. The LEFT edge is darker than any fill and every other
    // line is a translucent white — slightly lighter than whatever it sits
    // on in either theme — which reads as a subtle bevel instead of a grid.
    const seamDark = "rgba(0, 0, 0, 0.5)";
    const seamLight = "rgba(255, 255, 255, 0.14)";
    const topLeft = (r: { x: number; y: number; w: number; h: number }) => {
      ctx.strokeStyle = seamDark;
      ctx.beginPath();
      ctx.moveTo(snap(r.x) + px / 2, snap(r.y + r.h));
      ctx.lineTo(snap(r.x) + px / 2, snap(r.y));
      ctx.stroke();
      ctx.strokeStyle = seamLight;
      ctx.beginPath();
      ctx.moveTo(snap(r.x), snap(r.y) + px / 2);
      ctx.lineTo(snap(r.x + r.w), snap(r.y) + px / 2);
      ctx.stroke();
    };

    // The separator that keeps nested hairlines from fusing into blobs: an
    // expanded folder's body is filled with this neutral tone, and since its
    // children tile 100% of the inner area, the fill only ever shows in the
    // 1px padding ring and the name strip — a light pixel between dark
    // lines, which is exactly how WizTree keeps its borders readable.
    const gap = "rgba(148, 152, 158, 0.55)";

    for (const r of rects) {
      if (r.is_dir && r.expanded) {
        ctx.fillStyle = gap;
        ctx.fillRect(r.x, r.y, r.w, r.h);
        // The name strip sits on the theme surface, not the light gap tone —
        // the gap color exists only to separate hairlines in the 1px padding.
        if (r.headed) {
          ctx.fillStyle = surface;
          ctx.fillRect(r.x + 1, r.y + 1, Math.max(0, r.w - 2), HEADER_PX);
        }
        // Folder ring: dark on the left like the boxes, light elsewhere.
        const rx = snap(r.x) + px / 2;
        const ry = snap(r.y) + px / 2;
        const rw = Math.max(0, snap(r.x + r.w) - snap(r.x) - px);
        const rh = Math.max(0, snap(r.y + r.h) - snap(r.y) - px);
        ctx.strokeStyle = seamDark;
        ctx.beginPath();
        ctx.moveTo(rx, ry + rh);
        ctx.lineTo(rx, ry);
        ctx.stroke();
        ctx.strokeStyle = seamLight;
        ctx.beginPath();
        ctx.moveTo(rx, ry);
        ctx.lineTo(rx + rw, ry);
        ctx.lineTo(rx + rw, ry + rh);
        ctx.lineTo(rx, ry + rh);
        ctx.stroke();

        if (r.headed) {
          const label = `${r.name}\\ (${r.allocated_display})`;
          ctx.fillStyle = headerText;
          ctx.save();
          ctx.beginPath();
          ctx.rect(r.x + 2, r.y + 1, r.w - 4, HEADER_PX);
          ctx.clip();
          ctx.fillText(label, r.x + 3, r.y + 2.5);
          ctx.restore();
        }
      } else {
        // Solid boxes: files, aggregated tails, and folders too small or
        // too deep to subdivide. Edge to edge, no labels.
        const dimmed =
          filterOn && !r.aggregate && !r.synthetic && !r.is_dir &&
          !filter.has(extensionOf(r.name));
        ctx.fillStyle = r.aggregate ? aggregateFill : leafColor(r, palette);
        if (dimmed) ctx.globalAlpha = 0.25;
        ctx.fillRect(r.x, r.y, r.w, r.h);
        ctx.globalAlpha = 1.0;
        topLeft(r);
      }
    }

    // Close the shared-edge scheme along the map's outer right/bottom.
    ctx.strokeStyle = seamLight;
    ctx.strokeRect(px / 2, px / 2, cssW - px, cssH - px);

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

  function extensionOf(name: string): string {
    const at = name.lastIndexOf(".");
    return at > 0 ? name.slice(at + 1).toLowerCase() : "";
  }

  function humanBytes(bytes: number): string {
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let v = bytes;
    let u = 0;
    while (v >= 1024 && u < units.length - 1) {
      v /= 1024;
      u += 1;
    }
    return u === 0 ? `${bytes} B` : `${v.toFixed(v >= 10 ? 0 : 1)} ${units[u]}`;
  }

  /** Legend entries for the active color mode. */
  const legend = $derived.by(() => {
    if (session.colorMode === "size") {
      let previous = 0;
      return session.sizeRanges.map((range, i) => {
        const label =
          i === session.sizeRanges.length - 1
            ? `> ${humanBytes(previous)}`
            : `≤ ${humanBytes(range.max_bytes)}`;
        previous = range.max_bytes;
        return { color: range.color, label };
      });
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
