//! Motion constants for Svelte JS transitions (in:fly, in:fade, ...).
//!
//! Svelte transitions take JS numbers, so they can't read the CSS `--duration-*`
//! / `--ease-*` tokens in theme.css via var(). This module is the JS-side
//! mirror of those tokens; keep the two in lockstep. CSS transitions (hover,
//! etc.) reference the CSS tokens directly - use these only where a transition
//! is driven from script. See STANDARDS sec 2 (Motion).

import { cubicOut } from "svelte/easing";

/** Milliseconds. Mirror of --duration-* in theme.css. */
export const DURATION = {
  fast: 140,
  base: 240,
} as const;

/** Per-item delay for list cascades. Mirror of --stagger-step. */
export const STAGGER_STEP = 40;

/** Default entrance easing. Pairs with --ease-out (decelerate). */
export const EASE_OUT = cubicOut;
