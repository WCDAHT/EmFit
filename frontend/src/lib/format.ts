/**
 * format.ts — THE size formatter. Every user-visible byte count in the app
 * goes through {@link formatSize}; nothing else may hand-format sizes (the
 * pre-formatted `*_display` strings the backend still ships are for the CLI
 * and are ignored by the frontend).
 *
 * Two regimes, controlled by the persisted `session.sizeUnit`:
 * - `"dynamic"` (default): the largest unit whose converted value is ≥ 1
 *   (a 900-byte file shows in B, a 3 MB one in MB).
 * - a fixed unit: everything renders in that one unit, byte or bit.
 *
 * Byte units step by 1024 (WizTree's convention, labeled KB/MB/…); bit
 * units are bytes×8 stepping by 1000 (network convention).
 */

import { session } from "./session.svelte";

export const SIZE_UNITS = [
  "dynamic",
  "B",
  "KB",
  "MB",
  "GB",
  "TB",
  "bit",
  "Kbit",
  "Mbit",
  "Gbit",
  "Tbit",
] as const;
export type SizeUnit = (typeof SIZE_UNITS)[number];

/** bytes-per-unit for byte units, bits-per-unit for bit units. */
const FACTOR: Record<Exclude<SizeUnit, "dynamic">, number> = {
  B: 1,
  KB: 1024,
  MB: 1024 ** 2,
  GB: 1024 ** 3,
  TB: 1024 ** 4,
  bit: 1,
  Kbit: 1e3,
  Mbit: 1e6,
  Gbit: 1e9,
  Tbit: 1e12,
};

const DYNAMIC_LADDER: Exclude<SizeUnit, "dynamic">[] = ["TB", "GB", "MB", "KB", "B"];

function inUnit(bytes: number, unit: Exclude<SizeUnit, "dynamic">): number {
  const base = unit.endsWith("bit") ? bytes * 8 : bytes;
  return base / FACTOR[unit];
}

/** Cached formatters: `toLocaleString` constructs a new `Intl.NumberFormat`
 *  on every call, which profiled as real money with the treemap formatting
 *  hundreds of labels per scene. One reused formatter per precision is ~10×
 *  cheaper with identical output. */
const formatters = new Map<number, Intl.NumberFormat>();
function formatter(digits: number): Intl.NumberFormat {
  let f = formatters.get(digits);
  if (!f) {
    f = new Intl.NumberFormat(undefined, {
      minimumFractionDigits: 0,
      maximumFractionDigits: digits,
    });
    formatters.set(digits, f);
  }
  return f;
}

/** Value → string with precision that matches its magnitude: whole numbers
 *  for B/bit and anything ≥ 100, one decimal ≥ 10, else two. Thousands get
 *  separators, so "everything in KB" stays readable on a 2 TB volume. */
function render(value: number, unit: Exclude<SizeUnit, "dynamic">): string {
  const whole = unit === "B" || unit === "bit" || value >= 100;
  const digits = whole ? 0 : value >= 10 ? 1 : 2;
  return `${formatter(digits).format(value)} ${unit}`;
}

/**
 * Format a byte count per the user's unit setting. Pass `unit` to override
 * (exports, tests); omit it to follow `session.sizeUnit` — call sites in
 * reactive contexts re-render automatically when the setting changes.
 */
export function formatSize(bytes: number, unit?: SizeUnit): string {
  const chosen = unit ?? session.sizeUnit;
  if (chosen !== "dynamic") return render(inUnit(bytes, chosen), chosen);
  for (const u of DYNAMIC_LADDER) {
    const value = inUnit(bytes, u);
    if (value >= 1) return render(value, u);
  }
  return `${bytes} B`;
}
