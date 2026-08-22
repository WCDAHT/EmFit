// State for the Advanced Search dialog.
//
// The dialog is a front end for the query syntax: it never filters anything
// itself, it writes a query into the search box. So all it owns is the form
// fields, one function that turns them into syntax, and one that reads syntax
// back into them.
//
// **Nothing typed is ever lost.** `load` offers each term to the panels; what
// no panel claims stays in `rest` and is written back out untouched. That is
// what lets a user type `regex:^\d+ size:>1gb`, open the dialog, tick a box,
// and still have their regex when they hit Search.

import {
  escapeQuotes,
  isDate,
  readFunction,
  readRange,
  readSize,
  rangeValue,
  sizeValue,
  splitTerms,
  unescapeQuotes,
  value,
  type SizeUnit,
} from "./advanced-syntax";

/** A text field with the three match toggles every one of them carries. */
export interface NameField {
  text: string;
  matchCase: boolean;
  wholeWords: boolean;
  diacritics: boolean;
}

function field(): NameField {
  return { text: "", matchCase: false, wholeWords: false, diacritics: false };
}

/** One date bound: kept even while switched off, so unticking a box does
 *  not throw the date away. */
export interface DateBound {
  on: boolean;
  date: string;
}

/** A date range (advanced-search.md C8). */
export interface DateFilter {
  from: DateBound;
  to: DateBound;
}

function dates(): DateFilter {
  return { from: { on: false, date: "" }, to: { on: false, date: "" } };
}

/** A size range, each end with its own unit. */
export interface SizeFilter {
  from: string;
  fromUnit: SizeUnit;
  to: string;
  toUnit: SizeUnit;
}

function sizes(): SizeFilter {
  return { from: "", fromUnit: "MB", to: "", toUnit: "MB" };
}

/** The Type dropdown (advanced-search.md C9), and the syntax each choice
 *  writes. "No extension" has no function of its own - it is a regex for a
 *  name with no dot in it, which is what having no extension means. */
export const TYPES = [
  { key: "any", label: "(All Files and Folders)", terms: [] },
  { key: "files", label: "All File Types", terms: ["file:"] },
  { key: "noext", label: "Files with no Extension", terms: ["file:", "regex:^[^.]+$"] },
  { key: "folders", label: "Folders", terms: ["folder:"] },
  { key: "empty", label: "Empty Folders", terms: ["empty:"] },
  { key: "roots", label: "Roots", terms: ["root:"] },
] as const;

export type TypeKey = (typeof TYPES)[number]["key"];

/** Where to look (advanced-search.md C7). */
export interface Located {
  path: string;
  subfolders: boolean;
}

/** Everything the dialog holds. Panels add their fields here as they land. */
export const advanced = $state({
  /** File names containing... (C6) */
  all: field(),
  phrase: field(),
  any: field(),
  none: field(),
  /** Located in... (C7) */
  located: { path: "", subfolders: true } as Located,
  /** Dates and size (C8) */
  modified: dates(),
  created: dates(),
  size: sizes(),
  /** Type (C9) */
  type: "any" as TypeKey,
  /** Terms no panel understands, preserved verbatim. */
  rest: "",
});

/** The modifier prefix a field's toggles produce, e.g. `case:ww:`. */
function prefix(f: NameField): string {
  return (
    (f.matchCase ? "case:" : "") +
    (f.wholeWords ? "ww:" : "") +
    (f.diacritics ? "diacritics:" : "")
  );
}

function words(f: NameField): string[] {
  return f.text.split(/\s+/).filter((w) => w !== "");
}

/** The query the dialog currently describes. */
export function buildQuery(): string {
  const parts: string[] = [];

  // The scope goes first: it is the one part of a query people read as a
  // heading rather than a condition.
  const path = advanced.located.path.trim().replace(/[\\/]+$/, "");
  if (path !== "") {
    parts.push(advanced.located.subfolders ? `\`${path}\`` : `infolder:${value(path)}`);
  }

  // Space is AND, so every word is simply its own term.
  parts.push(...words(advanced.all).map((w) => prefix(advanced.all) + w));

  const phrase = advanced.phrase.text.trim();
  if (phrase !== "") parts.push(`${prefix(advanced.phrase)}"${escapeQuotes(phrase)}"`);

  // OR binds looser than AND, so a multi-word any-of has to be grouped or it
  // would swallow the terms around it.
  const any = words(advanced.any).map((w) => prefix(advanced.any) + w);
  if (any.length === 1) parts.push(any[0]);
  else if (any.length > 1) parts.push(`<${any.join(" | ")}>`);

  parts.push(...words(advanced.none).map((w) => `!${prefix(advanced.none)}${w}`));

  parts.push(...(TYPES.find((t) => t.key === advanced.type)?.terms ?? []));

  push(parts, "dm", dateRange(advanced.modified));
  push(parts, "dc", dateRange(advanced.created));
  push(parts, "size", sizeRange(advanced.size));

  if (advanced.rest !== "") parts.push(advanced.rest);
  return parts.join(" ");
}

/** Add `name:value`, if there is a value. */
function push(parts: string[], name: string, value: string | undefined) {
  if (value !== undefined) parts.push(`${name}:${value}`);
}

function dateRange(f: DateFilter): string | undefined {
  return rangeValue(f.from.on ? f.from.date : "", f.to.on ? f.to.date : "");
}

function sizeRange(f: SizeFilter): string | undefined {
  return rangeValue(
    f.from.trim() === "" ? "" : sizeValue(f.from, f.fromUnit),
    f.to.trim() === "" ? "" : sizeValue(f.to, f.toUnit),
  );
}

/** Fill the dialog from a query string. */
export function load(text: string) {
  reset();
  const rest: string[] = [];
  for (const term of splitTerms(text)) {
    if (!claim(term)) rest.push(term);
  }
  advanced.rest = rest.join(" ");
}

/** Clear every field. */
export function reset() {
  advanced.all = field();
  advanced.phrase = field();
  advanced.any = field();
  advanced.none = field();
  advanced.located = { path: "", subfolders: true };
  advanced.modified = dates();
  advanced.created = dates();
  advanced.size = sizes();
  advanced.type = "any";
  advanced.rest = "";
}

/** Try to read one term into a panel. False means "not mine" - the term goes
 *  back to the search box untouched. */
function claim(term: string): boolean {
  if (claimLocated(term) || claimType(term) || claimDates(term) || claimSize(term)) {
    return true;
  }
  if (term.startsWith("!")) return into(advanced.none, term.slice(1));

  if (term.startsWith("<") && term.endsWith(">")) {
    const inner = term
      .slice(1, -1)
      .split("|")
      .map((part) => part.trim());
    // All or nothing, and checked before anything is written: a group this
    // panel only half understands must come back out exactly as it went in.
    if (inner.length < 2 || !inner.every(readable)) return false;
    const first = strip(inner[0]).mods;
    if (!inner.every((part) => same(strip(part).mods, first))) return false;
    if (advanced.any.text !== "" && !same(advanced.any, first)) return false;
    inner.forEach((part) => put(advanced.any, part));
    return true;
  }

  if (strip(term).body.startsWith('"')) return into(advanced.phrase, term);
  return into(advanced.all, term);
}

/** Located in: a backtick scope searches subfolders, `infolder:` does not.
 *  Only the first one is taken; a second scope is left in the search box. */
function claimLocated(term: string): boolean {
  if (advanced.located.path !== "") return false;

  const inFolder = readFunction(term, ["infolder:", "parent:"]);
  const path = term.startsWith("`") ? term.replace(/^`|`$/g, "") : inFolder;
  if (path === undefined || path.trim() === "") return false;

  advanced.located = { path, subfolders: inFolder === undefined };
  return true;
}

/** The Type dropdown, recognized only in the exact terms it writes.
 *
 *  "No extension" is two terms, and they arrive one at a time: `file:` sets
 *  the dropdown to All File Types, and the regex that follows upgrades it. */
function claimType(term: string): boolean {
  const lower = term.toLowerCase();
  if (advanced.type === "files" && term === "regex:^[^.]+$") {
    advanced.type = "noext";
    return true;
  }
  if (advanced.type !== "any") return false;

  const match = TYPES.find((t) => t.terms.length === 1 && t.terms[0] === lower);
  if (!match) return false;
  advanced.type = match.key;
  return true;
}

/** Date modified and date created, in the shapes the pickers write. */
function claimDates(term: string): boolean {
  const modified = readFunction(term, ["datemodified:", "dm:"]);
  if (modified !== undefined) return intoDates(advanced.modified, modified);
  const created = readFunction(term, ["datecreated:", "dc:"]);
  if (created !== undefined) return intoDates(advanced.created, created);
  return false;
}

function intoDates(f: DateFilter, text: string): boolean {
  if (f.from.on || f.to.on) return false;
  const range = readRange(text, isDate);
  if (range === undefined) return false;
  f.from = { on: range.from !== "", date: range.from };
  f.to = { on: range.to !== "", date: range.to };
  return true;
}

function claimSize(term: string): boolean {
  const text = readFunction(term, ["size:"]);
  if (text === undefined) return false;
  if (advanced.size.from !== "" || advanced.size.to !== "") return false;

  const range = readRange(text, (part) => readSize(part) !== undefined);
  if (range === undefined) return false;
  const from = range.from === "" ? undefined : readSize(range.from);
  const to = range.to === "" ? undefined : readSize(range.to);

  advanced.size = {
    from: from?.amount ?? "",
    fromUnit: from?.unit ?? "MB",
    to: to?.amount ?? "",
    toUnit: to?.unit ?? "MB",
  };
  return true;
}

type Toggles = Omit<NameField, "text">;

function same(a: Toggles, b: Toggles): boolean {
  return (
    a.matchCase === b.matchCase &&
    a.wholeWords === b.wholeWords &&
    a.diacritics === b.diacritics
  );
}

/** Peel the modifiers this panel knows off a term. */
function strip(term: string): { mods: Toggles; body: string } {
  const mods: Toggles = { matchCase: false, wholeWords: false, diacritics: false };
  let body = term;
  for (;;) {
    if (/^case:/i.test(body)) {
      mods.matchCase = true;
      body = body.slice(5);
    } else if (/^ww:/i.test(body)) {
      mods.wholeWords = true;
      body = body.slice(3);
    } else if (/^wholeword:/i.test(body)) {
      mods.wholeWords = true;
      body = body.slice(10);
    } else if (/^diacritics:/i.test(body)) {
      mods.diacritics = true;
      body = body.slice(11);
    } else {
      return { mods, body };
    }
  }
}

/** The text a term searches for, or "" when it is not a plain name search:
 *  a function, a modifier with no checkbox here, a path scope, a group. */
function textOf(term: string): string {
  const { body } = strip(term);
  if (body === "" || body.startsWith("`") || body.startsWith("<")) return "";
  if (body.startsWith('"')) return unescapeQuotes(body.replace(/^"|"$/g, ""));
  // A colon outside a phrase means a function or an unknown modifier.
  return body.includes(":") ? "" : body;
}

function readable(term: string): boolean {
  return textOf(term) !== "";
}

/** Put a term in a field, if the field can hold it. A field takes its toggles
 *  from the first term it claims; a later term whose modifiers disagree is
 *  left in the search box rather than silently re-flagged. */
function into(f: NameField, term: string): boolean {
  if (!readable(term)) return false;
  // The phrase field holds one phrase, not a list.
  if (f === advanced.phrase && f.text !== "") return false;
  if (f.text !== "" && !same(f, strip(term).mods)) return false;
  put(f, term);
  return true;
}

function put(f: NameField, term: string) {
  const { mods } = strip(term);
  if (f.text === "") {
    f.matchCase = mods.matchCase;
    f.wholeWords = mods.wholeWords;
    f.diacritics = mods.diacritics;
  }
  const text = textOf(term);
  f.text = f.text === "" ? text : `${f.text} ${text}`;
}
