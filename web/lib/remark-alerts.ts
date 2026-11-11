// Remark plugin: the alert syntax GitHub renders, as the site's callouts, so a page reads the same
// in both places.
//
//   > [!NOTE]            ->  <Callout type="info"  title="Note">
//   > A store is ...           A store is ...
//
// The three words the docs use are the only ones: any other `[!WORD]` fails the build, naming the
// file, rather than show a stray marker on the site.
import * as path from 'node:path';
import type { Blockquote, Paragraph, Root } from 'mdast';
import type { MdxJsxFlowElement } from 'mdast-util-mdx-jsx';
import type { VFile } from 'vfile';
import { visit } from 'unist-util-visit';

export interface AlertsOptions {
  /** Absolute path of the repository root, to name a file in an error. */
  root: string;
}

interface Alert {
  type: 'info' | 'warn' | 'error';
  title: string;
}

const ALERTS: Record<string, Alert> = {
  NOTE: { type: 'info', title: 'Note' },
  WARNING: { type: 'warn', title: 'Caution' },
  CAUTION: { type: 'error', title: 'Danger' },
};

// `[!NOTE]` at the start of a quote's first line, and the line break after it.
const MARKER = /^\[!([A-Za-z]+)\][ \t]*(\r?\n|$)/;

/** The alert word of a quote's first paragraph, or null for a plain quote. */
function alertWord(quote: Blockquote): { word: string; first: Paragraph; length: number } | null {
  const first = quote.children[0];
  if (first?.type !== 'paragraph') {
    return null;
  }
  const text = first.children[0];
  if (text?.type !== 'text') {
    return null;
  }
  const match = MARKER.exec(text.value);
  return match ? { word: match[1], first, length: match[0].length } : null;
}

export function remarkAlerts({ root }: AlertsOptions) {
  return (tree: Root, file: VFile) => {
    const name = path
      .relative(root, file.path)
      .split(path.sep)
      .join('/');

    visit(tree, 'blockquote', (quote, index, parent) => {
      const found = alertWord(quote);
      if (!found || !parent || index === undefined) {
        return;
      }

      // 1. Only the three known words.
      const alert = ALERTS[found.word.toUpperCase()];
      if (!alert) {
        throw new Error(`remark-alerts: ${name}: [!${found.word}] is not one of [!NOTE], [!WARNING], [!CAUTION]`);
      }

      // 2. The marker leaves the text; a paragraph left empty leaves the quote.
      const text = found.first.children[0];
      if (text.type === 'text') {
        text.value = text.value.slice(found.length);
        if (text.value === '') {
          found.first.children.shift();
        }
      }
      const body = found.first.children.length === 0 ? quote.children.slice(1) : quote.children;

      // 3. The quote becomes the callout, its blocks unchanged.
      const callout: MdxJsxFlowElement = {
        type: 'mdxJsxFlowElement',
        name: 'Callout',
        attributes: [
          { type: 'mdxJsxAttribute', name: 'type', value: alert.type },
          { type: 'mdxJsxAttribute', name: 'title', value: alert.title },
        ],
        children: body,
      };
      parent.children[index] = callout;
    });
  };
}
