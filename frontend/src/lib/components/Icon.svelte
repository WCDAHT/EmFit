<!--
  Icon.svelte - thin wrapper over the Bootstrap Icons webfont (STANDARDS sec 2.5).

  Usage:  <Icon name="folder2-open" />
          <Icon name="trash" size={20} color="var(--danger)" />

  `name` is the Bootstrap Icons id without the `bi-` prefix. Size defaults to
  1em so the glyph tracks the surrounding text; pass a number for a fixed px
  size. Color defaults to currentColor (inherits text color).

  For icons that need per-path styling or animation (e.g. Svelte Flow nodes),
  import the raw SVG instead: `import folder from "bootstrap-icons/icons/folder2-open.svg?raw"`.
-->
<script lang="ts">
  interface Props {
    name: string;
    size?: number | string;
    color?: string;
    title?: string;
  }

  let { name, size = "1em", color = "currentColor", title }: Props = $props();

  const dim = $derived(typeof size === "number" ? `${size}px` : size);
</script>

<i
  class="bi bi-{name}"
  style:font-size={dim}
  style:color
  style:line-height="1"
  aria-hidden={title ? undefined : "true"}
  aria-label={title}
  {title}
></i>
