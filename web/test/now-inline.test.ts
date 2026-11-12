// Unit tests of web/components/now/inline.ts, the inline renderer of /now's CHANGELOG entries:
//
//   node --experimental-strip-types --test 'web/test/*.test.ts'
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { InlineKind, inlineParts } from '../components/now/inline.ts';

// Stands in for rewriteLink('CHANGELOG.md', href), which needs the docs navigation file.
const rewrite = (href: string) => `/rewritten/${href}`;

test('code_span_rendered', () => {
  // A code span is code, even when it holds link syntax.
  assert.deepEqual(inlineParts('Run `so land` or `[a](b)` now.', rewrite), [
    { kind: InlineKind.Text, text: 'Run ' },
    { kind: InlineKind.Code, text: 'so land' },
    { kind: InlineKind.Text, text: ' or ' },
    { kind: InlineKind.Code, text: '[a](b)' },
    { kind: InlineKind.Text, text: ' now.' },
  ]);
});

test('relative_link_rewritten', () => {
  // The href goes through the rewrite; the link text keeps its own code spans.
  assert.deepEqual(inlineParts('See [`RELEASE.md`](RELEASE.md#cut).', rewrite), [
    { kind: InlineKind.Text, text: 'See ' },
    {
      kind: InlineKind.Link,
      href: '/rewritten/RELEASE.md#cut',
      parts: [{ kind: InlineKind.Code, text: 'RELEASE.md' }],
    },
    { kind: InlineKind.Text, text: '.' },
  ]);
});
