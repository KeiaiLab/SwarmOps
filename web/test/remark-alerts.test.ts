// Unit tests of web/lib/remark-alerts.ts, the callout path no docs page uses yet:
//
//   node --experimental-strip-types --test 'web/test/*.test.ts'
import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { Blockquote, Root } from 'mdast';
import type { VFile } from 'vfile';

import { remarkAlerts } from '../lib/remark-alerts.ts';

const ROOT = '/repo';
const FILE = { path: '/repo/docs/guides/a.md' } as VFile;

/** `> [!WORD]` and its lines as the parser gives them: one paragraph, one text node. */
function quote(text: string): Blockquote {
  return { type: 'blockquote', children: [{ type: 'paragraph', children: [{ type: 'text', value: text }] }] };
}

/** The tree after the plugin ran over a page holding these blocks. */
function run(...children: Root['children']): Root {
  const tree: Root = { type: 'root', children };
  remarkAlerts({ root: ROOT })(tree, FILE);
  return tree;
}

test('note_becomes_an_info_callout_without_its_marker', () => {
  const [callout] = run(quote('[!NOTE]\nA store is one directory.')).children;
  assert.deepEqual(callout, {
    type: 'mdxJsxFlowElement',
    name: 'Callout',
    attributes: [
      { type: 'mdxJsxAttribute', name: 'type', value: 'info' },
      { type: 'mdxJsxAttribute', name: 'title', value: 'Note' },
    ],
    children: [{ type: 'paragraph', children: [{ type: 'text', value: 'A store is one directory.' }] }],
  });
});

test('warning_and_caution_take_their_titles', () => {
  const titles = run(quote('[!WARNING]\nA.'), quote('[!caution]\nB.')).children.map((node) =>
    node.type === 'mdxJsxFlowElement' ? node.attributes.map((a) => ('value' in a ? a.value : '')).join(' ') : node.type,
  );
  assert.deepEqual(titles, ['warn Caution', 'error Danger']);
});

test('a_marker_alone_leaves_no_empty_paragraph', () => {
  const body: Blockquote = {
    type: 'blockquote',
    children: [
      { type: 'paragraph', children: [{ type: 'text', value: '[!NOTE]' }] },
      { type: 'paragraph', children: [{ type: 'text', value: 'Second block.' }] },
    ],
  };
  const [callout] = run(body).children;
  assert.equal(callout.type, 'mdxJsxFlowElement');
  assert.deepEqual(callout.type === 'mdxJsxFlowElement' ? callout.children : [], [
    { type: 'paragraph', children: [{ type: 'text', value: 'Second block.' }] },
  ]);
});

test('a_plain_quote_stays_a_quote', () => {
  const plain = quote('Quoted, no marker.');
  assert.deepEqual(run(structuredClone(plain)).children, [plain]);
});

test('an_unknown_word_fails_naming_the_file', () => {
  assert.throws(() => run(quote('[!TIP]\nA.')), /remark-alerts: docs\/guides\/a\.md: \[!TIP\] is not one of/);
});
