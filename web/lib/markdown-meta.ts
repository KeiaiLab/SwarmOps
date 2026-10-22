// Title and description of a Markdown file without front matter: its first `# ` heading and the
// first paragraph after it, as plain text. Repository docs carry no front matter (they read the
// same on GitHub), so the site takes both from the text.
const DESCRIPTION_MAX = 160;
const ELLIPSIS = '…';

// Lines that open a block other than a paragraph: heading, fence, list, quote, table, rule, HTML.
const NON_PARAGRAPH = /^(#|```|~~~|[-*+] |\d+[.)] |>|\||---|<)/;

/** Markdown inline syntax to plain text: `[a](b)` -> a, `` `E_*` `` -> E_*, `**y**` -> y. */
function plainText(markdown: string): string {
  // 1. Code spans aside, so their `_` and `*` survive the emphasis step.
  const spans: string[] = [];
  const text = markdown.replace(/`([^`]*)`/g, (_, code: string) => `\u0000${spans.push(code) - 1}\u0000`);

  // 2. Links to their text, emphasis to its content, whitespace to one space.
  return text
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/!?\[([^\]]*)\]\[[^\]]*\]/g, '$1')
    .replace(/(\*\*|__)(.+?)\1/g, '$2')
    .replace(/(^|[^\w*])[*_]([^*_\s][^*_]*)[*_](?![\w*])/g, '$1$2')
    .replace(/\u0000(\d+)\u0000/g, (_, i: string) => spans[Number(i)])
    .replace(/\s+/g, ' ')
    .trim();
}

/** The first `# ` heading, or `fallback`. */
export function firstHeading(source: string, fallback: string): string {
  const match = /^# (.+)$/m.exec(source);
  return match ? plainText(match[1]) : fallback;
}

/** The first paragraph after the first heading, cut to a meta description's length. */
export function firstParagraph(source: string): string | undefined {
  const lines = source.split('\n');

  // 1. Skip to the first paragraph line after the title.
  const titleAt = lines.findIndex((line) => line.startsWith('# '));
  let at = titleAt + 1;
  while (at < lines.length && (lines[at].trim() === '' || NON_PARAGRAPH.test(lines[at]))) {
    at += 1;
  }

  // 2. The paragraph runs to the next blank line.
  const paragraph: string[] = [];
  while (at < lines.length && lines[at].trim() !== '') {
    paragraph.push(lines[at]);
    at += 1;
  }
  const text = plainText(paragraph.join(' '));
  if (text === '') {
    return undefined;
  }

  // 3. Cut on a word boundary.
  if (text.length <= DESCRIPTION_MAX) {
    return text;
  }
  const cut = text.slice(0, DESCRIPTION_MAX - ELLIPSIS.length);
  return cut.slice(0, cut.lastIndexOf(' ')) + ELLIPSIS;
}
