// The inline Markdown of one CHANGELOG entry, as parts /now renders. No imports, so node's test
// runner loads this file as it is (web/test/now-inline.test.ts):
//
//   'See [`RELEASE.md`](RELEASE.md).'
//     -> text 'See ', link(rewrite('RELEASE.md'), [code 'RELEASE.md']), text '.'
//
// Code spans and links only: all an entry uses. Anything else prints as written.

/** What a part of an entry is. */
export const InlineKind = {
  Text: 'text',
  Code: 'code',
  Link: 'link',
} as const;
export type InlineKind = (typeof InlineKind)[keyof typeof InlineKind];

export type InlinePart =
  | { kind: typeof InlineKind.Text; text: string }
  | { kind: typeof InlineKind.Code; text: string }
  | { kind: typeof InlineKind.Link; href: string; parts: InlinePart[] };

// A code span first, so link syntax inside one stays code.
const INLINE = /`([^`]+)`|\[([^\]]+)\]\(([^)\s]+)\)/g;

/** `text` split into text, code spans and links; each link's href goes through `rewrite`. */
export function inlineParts(text: string, rewrite: (href: string) => string): InlinePart[] {
  const parts: InlinePart[] = [];
  let at = 0;

  // Plain text between marks; empty runs are dropped.
  const plain = (end: number) => {
    if (end > at) {
      parts.push({ kind: InlineKind.Text, text: text.slice(at, end) });
    }
  };

  for (const match of text.matchAll(INLINE)) {
    const [whole, code, linkText, href] = match;
    plain(match.index);
    at = match.index + whole.length;

    // One mark per match: a code span, or a link whose text may hold code.
    if (code !== undefined) {
      parts.push({ kind: InlineKind.Code, text: code });
      continue;
    }
    parts.push({ kind: InlineKind.Link, href: rewrite(href), parts: inlineParts(linkText, rewrite) });
  }
  plain(text.length);
  return parts;
}
