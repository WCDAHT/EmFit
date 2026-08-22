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

/** A value that has to survive as one term even with spaces in it. Only
 *  quoted when it needs to be - `infolder:C:\\Users` reads better bare. */
function value(text: string): string {
  return /[\s"]/.test(text) ? `"${escapeQuotes(text)}"` : text;
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

  if (advanced.rest !== "") parts.push(advanced.rest);
  return parts.join(" ");
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
  advanced.rest = "";
}

/** Try to read one term into a panel. False means "not mine" - the term goes
 *  back to the search box untouched. */
function claim(term: string): boolean {
  if (claimLocated(term)) return true;
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

/** The value of a function term, or undefined when the term is not one of
 *  these functions. Quoted values come back unquoted. */
export function readFunction(term: string, names: string[]): string | undefined {
  const lower = term.toLowerCase();
  const name = names.find((n) => lower.startsWith(n));
  if (name === undefined) return undefined;
  const raw = term.slice(name.length);
  return raw.startsWith('"') ? unescapeQuotes(raw.replace(/^"|"$/g, "")) : raw;
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
function escapeQuotes(text: string): string {
  return text.replaceAll('"', "quot:");
}

function unescapeQuotes(text: string): string {
  return text.replaceAll("quot:", '"');
}
