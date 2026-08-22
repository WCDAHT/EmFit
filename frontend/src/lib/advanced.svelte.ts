// State for the Advanced Search dialog.
//
// The dialog is a front end for the query syntax: it never filters anything
// itself, it writes a query into the search box. So all it owns is the form
// fields, one function that turns them into syntax, and one that reads syntax
// back into them.
//
// **Nothing typed is ever lost.** `load` hands each term to the panels; what
// no panel claims stays in `rest` and is written back out untouched. That is
// what lets a user type `regex:^\d+ size:>1gb`, open the dialog, tick a box,
// and still have their regex when they hit Search.

/** Everything the dialog holds. Panels add their fields here as they land. */
export const advanced = $state({
  /** Terms no panel understands, preserved verbatim. */
  rest: "",
});

/** The query the dialog currently describes. */
export function buildQuery(): string {
  return [advanced.rest].filter((part) => part !== "").join(" ");
}

/** Fill the dialog from a query string. */
export function load(text: string) {
  const claimed: string[] = [];
  advanced.rest = splitTerms(text)
    .filter((term) => !claimed.includes(term))
    .join(" ");
}

/** Clear every field. */
export function reset() {
  advanced.rest = "";
}

/** Split a query into top-level terms, the way the Rust tokenizer does:
 *  whitespace separates, but not inside `"quotes"` or `` `backticks` ``. */
export function splitTerms(text: string): string[] {
  const terms: string[] = [];
  let current = "";
  let closer = "";

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
    if (/\s/.test(c)) {
      if (current !== "") terms.push(current);
      current = "";
      continue;
    }
    current += c;
  }
  if (current !== "") terms.push(current);
  return terms;
}

/** Quote a value that has to survive as one term. */
export function quoted(text: string): string {
  return /\s/.test(text) ? `"${text}"` : text;
}
