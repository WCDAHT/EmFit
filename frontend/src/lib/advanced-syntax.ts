// Reading and writing query syntax for the Advanced Search dialog.
//
// Pure string work, no state: `advanced.svelte.ts` owns the form fields and
// calls in here to turn them into terms and back. Kept separate because the
// two jobs grow at different rates - every new panel adds fields, but nearly
// all of them reuse the range and function helpers below.

/** Split a query into top-level terms, the way the Rust tokenizer does:
 *  whitespace separates, but not inside `"quotes"`, `` `backticks` ``, or
 *  `<groups>`. */
export function splitTerms(text: string): string[] {
  const terms: string[] = [];
  let current = "";
  let closer = "";
  let depth = 0;

  for (const c of text) {
    if (closer) {
      current += c;
      if (c === closer) closer = "";
      continue;
    }
    if (c === '"' || c === "`") {
      closer = c;
      current += c;
      continue;
    }
    if (c === "<") depth += 1;
    if (c === ">") depth = Math.max(0, depth - 1);
    if (depth === 0 && /\s/.test(c)) {
      if (current !== "") terms.push(current);
      current = "";
      continue;
    }
    current += c;
  }
  if (current !== "") terms.push(current);
  return terms;
}

/** A quote inside a phrase is written with the `quot:` macro, which is what
 *  the parser expands it back to. */
export function escapeQuotes(text: string): string {
  return text.replaceAll('"', "quot:");
}

export function unescapeQuotes(text: string): string {
  return text.replaceAll("quot:", '"');
}

/** A value that has to survive as one term even with spaces in it. Only
 *  quoted when it needs to be - `infolder:C:\Users` reads better bare. */
export function value(text: string): string {
  return /[\s"]/.test(text) ? `"${escapeQuotes(text)}"` : text;
}

/** The value of a function term, or undefined when the term is not one of
 *  these functions. Quoted values come back unquoted. */
export function readFunction(term: string, names: string[]): string | undefined {
  const lower = term.toLowerCase();
  const name = names.find((n) => lower.startsWith(n));
  if (name === undefined) return undefined;
  const raw = term.slice(name.length);
  return raw.startsWith('"') ? unescapeQuotes(raw.replace(/^"|"$/g, "")) : raw;
}

/** A from/to pair, either half of which may be blank. */
export interface Range {
  from: string;
  to: string;
}

/** The comparison a from/to pair writes: `a..b`, `>=a`, `<=b`, or nothing. */
export function rangeValue(from: string, to: string): string | undefined {
  if (from !== "" && to !== "") return `${from}..${to}`;
  if (from !== "") return `>=${from}`;
  if (to !== "") return `<=${to}`;
  return undefined;
}

/** Read back what `rangeValue` writes, and nothing else.
 *
 *  Deliberately narrow: a hand-written `size:>1mb..<2gb` or a size constant
 *  has no from/to boxes to land in, so it stays in the search box rather than
 *  being approximated into the form and quietly changed on the way out. */
export function readRange(text: string, valid: (part: string) => boolean): Range | undefined {
  const bounded = text.split("..");
  if (bounded.length === 2) {
    const [from, to] = bounded.map((p) => p.trim());
    return valid(from) && valid(to) ? { from, to } : undefined;
  }
  const at_least = text.match(/^>=?\s*(.+)$/);
  if (at_least && valid(at_least[1].trim())) return { from: at_least[1].trim(), to: "" };
  const at_most = text.match(/^<=?\s*(.+)$/);
  if (at_most && valid(at_most[1].trim())) return { from: "", to: at_most[1].trim() };
  return undefined;
}

/** `2024-06-15`, the only date shape the pickers produce. */
export function isDate(text: string): boolean {
  return /^\d{4}-\d{2}-\d{2}$/.test(text);
}

/** A whole number, for the count fields. */
export function isCount(text: string): boolean {
  return /^\d+$/.test(text);
}

/** The size units the dialog offers. Binary, matching the display side. */
export const SIZE_UNITS = ["B", "KB", "MB", "GB"] as const;
export type SizeUnit = (typeof SIZE_UNITS)[number];

/** A number and a unit, as one size term value. */
export function sizeValue(amount: string, unit: SizeUnit): string {
  return `${amount.trim()}${unit.toLowerCase()}`;
}

/** Split `1.5gb` back into its number and unit. */
export function readSize(text: string): { amount: string; unit: SizeUnit } | undefined {
  const parts = text.match(/^(\d+(?:\.\d+)?)\s*(b|kb|mb|gb)?$/i);
  if (!parts) return undefined;
  const unit = (parts[2] ?? "B").toUpperCase() as SizeUnit;
  return { amount: parts[1], unit };
}
