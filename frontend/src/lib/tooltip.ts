// Hover toasts for the Advanced Search dialog.
//
// Every control in that dialog explains itself on hover (advanced-search.md,
// the standing rule for the whole feature). `title` attributes would be the
// cheap way, but they take a second to appear, cannot be styled with the
// design tokens, and never show on keyboard focus - so this is a Svelte
// action instead:
//
//   <input use:tip={"What this field does. Writes: case:report"} />
//
// One element is shared by every tooltip on the page and parked on <body>, so
// no dialog needs to reserve room for it or worry about overflow clipping.

/** How far the toast sits from the control it explains. */
const GAP = 8;

let toast: HTMLDivElement | undefined;

function element(): HTMLDivElement {
  if (!toast) {
    toast = document.createElement("div");
    toast.className = "tip-toast";
    toast.setAttribute("role", "tooltip");
    document.body.appendChild(toast);
  }
  return toast;
}

function show(anchor: HTMLElement, text: string) {
  if (!text) return;
  const node = element();
  node.textContent = text;
  node.style.visibility = "hidden";
  node.style.display = "block";

  // Below the control by default, above it when that would run off-screen.
  const box = anchor.getBoundingClientRect();
  const size = node.getBoundingClientRect();
  const below = box.bottom + GAP;
  const top = below + size.height > window.innerHeight ? box.top - size.height - GAP : below;
  const left = Math.max(GAP, Math.min(box.left, window.innerWidth - size.width - GAP));

  node.style.top = `${Math.max(GAP, top)}px`;
  node.style.left = `${left}px`;
  node.style.visibility = "visible";
}

function hide() {
  if (toast) toast.style.display = "none";
}

/** Explain a control on hover and on keyboard focus. */
export function tip(node: HTMLElement, text: string) {
  let current = text;

  const enter = () => show(node, current);
  const leave = () => hide();

  node.addEventListener("mouseenter", enter);
  node.addEventListener("mouseleave", leave);
  node.addEventListener("focusin", enter);
  node.addEventListener("focusout", leave);

  return {
    update(next: string) {
      current = next;
      if (toast?.style.display === "block") show(node, current);
    },
    destroy() {
      node.removeEventListener("mouseenter", enter);
      node.removeEventListener("mouseleave", leave);
      node.removeEventListener("focusin", enter);
      node.removeEventListener("focusout", leave);
      hide();
    },
  };
}
