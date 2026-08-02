/** Indentation width of a line: tabs count as 4, spaces count as 1. */
function indentWidth(line: string): number {
  let w = 0;
  for (const ch of line) {
    if (ch === "\t") w += 4;
    else if (ch === " ") w += 1;
    else break;
  }
  return w;
}

/**
 * Parse pasted text — single names, slash paths, or an indented hierarchy — into
 * full tag paths (one per non-empty line, parents included).
 *
 * Algorithm:
 *   - Maintain a stack of { indent, name } entries.
 *   - For each non-blank line, compute its indent width.
 *   - Pop the stack while the top entry's indent >= the current line's indent
 *     (i.e. we've dedented to this level or beyond).
 *   - Push the current { indent, name: trimmedLine } onto the stack.
 *   - Emit the full path: stack names joined by "/".
 *   - Lines that contain "/" or "|" are left as-is — the backend splits them.
 *   - De-duplicate the output while preserving order.
 */
export function parseTagPaste(text: string): string[] {
  if (!text || !text.trim()) return [];

  const stack: Array<{ indent: number; name: string }> = [];
  const seen = new Set<string>();
  const result: string[] = [];

  for (const rawLine of text.split("\n")) {
    // Skip blank / whitespace-only lines.
    if (!rawLine.trim()) continue;

    const indent = indentWidth(rawLine);
    const name = rawLine.trim();

    // Pop stack entries that are at the same or greater indent level.
    while (stack.length > 0 && stack[stack.length - 1].indent >= indent) {
      stack.pop();
    }

    stack.push({ indent, name });

    const fullPath = stack.map((e) => e.name).join("/");
    if (!seen.has(fullPath)) {
      seen.add(fullPath);
      result.push(fullPath);
    }
  }

  return result;
}
