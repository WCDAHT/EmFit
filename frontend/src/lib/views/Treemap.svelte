<!--
  Treemap.svelte — the WizTree-style map (features.md §4.2).

  All layout happens in Rust: this component sends the canvas size, drill
  point, and depth, and paints the flat rectangle list it gets back. Painting
  a few thousand precomputed rects is cheap; recomputing them on resize or
  drill is one IPC call under the core's 100 ms budget.

  Interactions: hover highlights and shows a tooltip (path fetched on
  demand), click focuses (and reveals in the folder tree), double-click
  drills into a directory with a short zoom animation, Backspace / Alt+Left
  or the breadcrumb go back up. Colors: by file type (category palette, with
  a legend) or by top-level folder. The synthetic free-space block renders
  muted so the map accounts for the whole volume without shouting about it.
-->
<script lang="ts">
  import { nodeInfo, nodeLineage, treemapLayout } from "../ipc";
  import { session } from "../session.svelte";
  import type { NodeInfoDto, TreemapRectDto } from "../types";

  interface Crumb {
    vol: number;
    id: number;
    name: string;
  }

  let wrap: HTMLDivElement | undefined = $state();
  let canvas: HTMLCanvasElement | undefined = $state();
  let cssW = $state(0);
  let cssH = $state(0);

  let rects: TreemapRectDto[] = $state([]);
  let crumbs: Crumb[] = $state([]);
  let hover: TreemapRectDto | null = $state(null);
  let hoverInfo: NodeInfoDto | null = $state(null);
  let tooltipXY = $state({ x: 0, y: 0 });

  const LEGEND: { label: string; slot: number }[] = [
    { label: "Folders", slot: 1 },
    { label: "Executables", slot: 2 },
    { label: "Archives", slot: 3 },
    { label: "Images", slot: 4 },
    { label: "Video", slot: 5 },
    { label: "Audio", slot: 6 },
    { label: "Documents", slot: 7 },
    { label: "Code", slot: 8 },
  ];

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

  // Repaint (no refetch) when only highlight inputs change.
  $effect(() => {
    void session.colorMode;
    void session.typeFilter.size;
    void session.focus;
    void hover;
    void rects;
    draw();
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
      }
    }
  }

  function cssVar(name: string): string {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  function fillFor(r: TreemapRectDto, palette: string[], dimmed: boolean): string {
    if (r.synthetic) return cssVar("--surface-hover");
    const slot =
      session.colorMode === "type" ? r.category : (r.branch % 8) + 1;
    const color = slot > 0 ? palette[slot - 1] : cssVar("--text-muted");
    return dimmed ? color + "33" : color;
  }

  function draw() {
    if (!canvas || cssW < 10 || cssH < 10) return;
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(cssW * dpr);
    canvas.height = Math.round(cssH * dpr);
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.scale(dpr, dpr);

    const palette = Array.from({ length: 8 }, (_, i) => cssVar(`--category-${i + 1}`));
    const border = cssVar("--border-strong");
    const surface = cssVar("--surface-sunken");
    const text = cssVar("--text-primary");
    const filterOn = session.typeFilter.size > 0;

    ctx.fillStyle = surface;
    ctx.fillRect(0, 0, cssW, cssH);

    for (const r of rects) {
      if (r.is_dir) {
        // Depth-shaded container: darker with depth, thin border.
        const shade = Math.min(0.28, 0.06 + r.depth * 0.04);
        ctx.fillStyle = `rgba(127, 127, 127, ${shade})`;
        ctx.fillRect(r.x, r.y, r.w, r.h);
        ctx.strokeStyle = border;
        ctx.lineWidth = 1;
        ctx.strokeRect(r.x + 0.5, r.y + 0.5, Math.max(0, r.w - 1), Math.max(0, r.h - 1));
      } else {
        const dimmed =
          filterOn && !session.typeFilter.has(extensionOf(r.name)) && !r.synthetic;
        ctx.fillStyle = fillFor(r, palette, dimmed);
        ctx.fillRect(r.x, r.y, r.w, r.h);
        ctx.strokeStyle = "rgba(0, 0, 0, 0.35)";
        ctx.lineWidth = 1;
        ctx.strokeRect(r.x + 0.5, r.y + 0.5, Math.max(0, r.w - 1), Math.max(0, r.h - 1));
      }
    }

    // Labels where they fit: directory name+size along the top edge, file
    // name inside large leaves.
    ctx.font = "11px Inter, sans-serif";
    ctx.textBaseline = "top";
    for (const r of rects) {
      if (r.w < 64 || r.h < 16) continue;
      const label = r.is_dir ? `${r.name}  ${r.allocated_display}` : r.name;
      ctx.fillStyle = r.is_dir ? text : "rgba(0, 0, 0, 0.75)";
      ctx.save();
      ctx.beginPath();
      ctx.rect(r.x + 2, r.y + 2, r.w - 4, 14);
      ctx.clip();
      ctx.fillText(label, r.x + 4, r.y + 3);
      ctx.restore();
    }

    // Focus and hover outlines last, on top.
    const outline = (r: TreemapRectDto, color: string, width: number) => {
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.strokeRect(r.x + 1, r.y + 1, Math.max(0, r.w - 2), Math.max(0, r.h - 2));
    };
    if (session.focus) {
      const f = rects.find((r) => r.vol === session.focus?.vol && r.id === session.focus?.id);
      if (f) outline(f, cssVar("--selection"), 2);
    }
    if (hover) outline(hover, cssVar("--text-primary"), 1.5);
  }

  function extensionOf(name: string): string {
    const at = name.lastIndexOf(".");
    return at > 0 ? name.slice(at + 1).toLowerCase() : "";
  }

  // -------------------------------------------------------------------------
  // hit testing + interactions
  // -------------------------------------------------------------------------

  /** Topmost = smallest rect under the cursor (children paint over parents). */
  function hitTest(x: number, y: number): TreemapRectDto | null {
    let best: TreemapRectDto | null = null;
    for (const r of rects) {
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
    tooltipXY = { x: e.clientX - (wrap?.getBoundingClientRect().left ?? 0), y: e.clientY - (wrap?.getBoundingClientRect().top ?? 0) };

    const hit = hitTest(x, y);
    if (hit?.id !== hover?.id || hit?.vol !== hover?.vol) {
      hover = hit;
      hoverInfo = null;
      clearTimeout(infoTimer);
      if (hit) {
        infoTimer = setTimeout(async () => {
          hoverInfo = await nodeInfo(hit.vol, hit.id);
        }, 120);
      }
    }
  }

  function onClick(e: MouseEvent) {
    const bounds = canvas?.getBoundingClientRect();
    if (!bounds) return;
    const hit = hitTest(e.clientX - bounds.left, e.clientY - bounds.top);
    if (hit) {
      session.focus = { vol: hit.vol, id: hit.id };
      session.revealEpoch += 1; // ask the folder tree to expand to it
    }
  }

  function onDblClick(e: MouseEvent) {
    const bounds = canvas?.getBoundingClientRect();
    if (!bounds) return;
    const hit = hitTest(e.clientX - bounds.left, e.clientY - bounds.top);
    if (hit?.is_dir && !hit.synthetic) {
      void drillTo({ vol: hit.vol, id: hit.id }, hit);
    }
  }

  /** Drill with a short zoom so the old and new frames visually connect. */
  async function drillTo(target: { vol: number; id: number }, from?: TreemapRectDto) {
    if (from && canvas) void animateZoom(from);
    session.drill = target;
    await rebuildCrumbs(target);
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
    session.drill = { vol: crumb.vol, id: crumb.id };
  }

  function drillUp() {
    if (crumbs.length > 1) {
      crumbClick(crumbs.length - 2);
    } else {
      crumbs = [];
      session.drill = null;
    }
  }

  /** ~150 ms scale of the current frame toward the target rect. */
  function animateZoom(target: TreemapRectDto): Promise<void> {
    return new Promise((resolve) => {
      if (!canvas) return resolve();
      const snapshot = document.createElement("canvas");
      snapshot.width = canvas.width;
      snapshot.height = canvas.height;
      snapshot.getContext("2d")?.drawImage(canvas, 0, 0);
      const ctx = canvas.getContext("2d");
      if (!ctx) return resolve();

      const start = performance.now();
      const dpr = window.devicePixelRatio || 1;
      const step = (now: number) => {
        const t = Math.min(1, (now - start) / 150);
        // Interpolate the viewport from the whole canvas to the target rect.
        const x = target.x * t;
        const y = target.y * t;
        const w = cssW - (cssW - target.w) * t;
        const h = cssH - (cssH - target.h) * t;
        ctx.save();
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        ctx.clearRect(0, 0, cssW, cssH);
        ctx.drawImage(
          snapshot,
          x * dpr,
          y * dpr,
          Math.max(1, w * dpr),
          Math.max(1, h * dpr),
          0,
          0,
          cssW,
          cssH,
        );
        ctx.restore();
        if (t < 1) {
          requestAnimationFrame(step);
        } else {
          resolve();
        }
      };
      requestAnimationFrame(step);
    });
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Backspace" || (e.altKey && e.key === "ArrowLeft")) {
      e.preventDefault();
      drillUp();
    }
  }

  // Drill changes made elsewhere (folder tree double-click) need crumbs too.
  $effect(() => {
    const drill = session.drill;
    if (drill && !crumbs.some((c) => c.vol === drill.vol && c.id === drill.id)) {
      void rebuildCrumbs(drill);
    }
    if (!drill) crumbs = [];
  });
</script>

<div class="treemap" bind:this={wrap}>
  <div class="crumbs">
    <button class="crumb" onclick={() => ((session.drill = null), (crumbs = []))}>
      All volumes
    </button>
    {#each crumbs as crumb, i (crumb.id)}
      <span class="sep">›</span>
      <button class="crumb" onclick={() => crumbClick(i)}>{crumb.name}</button>
    {/each}
    <span class="flex"></span>
    {#if session.colorMode === "type"}
      <div class="legend">
        {#each LEGEND as item (item.slot)}
          <span class="swatch" style:background="var(--category-{item.slot})"></span>
          <span class="legend-label">{item.label}</span>
        {/each}
      </div>
    {/if}
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
      style:width="{cssW}px"
      style:height="{cssH}px"
      tabindex="0"
      onmousemove={onMouseMove}
      onmouseleave={() => (hover = null)}
      onclick={onClick}
      ondblclick={onDblClick}
      onkeydown={onKeydown}
    ></canvas>

    {#if hover}
      <div
        class="tooltip"
        style:left="{Math.min(tooltipXY.x + 14, cssW - 260)}px"
        style:top="{Math.min(tooltipXY.y + 14, cssH - 60)}px"
      >
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
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    overflow: hidden;
    background: var(--surface-sunken);
  }
  canvas {
    display: block;
    outline: none;
  }

  .tooltip {
    position: absolute;
    z-index: 10;
    max-width: 260px;
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
