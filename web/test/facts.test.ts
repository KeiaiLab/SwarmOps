// Unit tests of web/lib/facts.ts, the pure CHANGELOG and cadence rules behind STABLE:
//
//   node --experimental-strip-types --test 'web/test/*.test.ts'
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { CadenceVerdict, cadence, parseUnreleased, releaseSection } from '../lib/facts.ts';

const LIMITS = { maxDays: 7, maxCommits: 200 };

// A changelog in the repository's shape: prose, Unreleased, then released sections.
const CHANGELOG = [
  '# Changelog',
  '',
  'Prose before the sections.',
  '',
  '## [Unreleased]',
  '',
  '### Fixed',
  '',
  '- First fix.',
  '',
  '### Added',
  '',
  '- First addition.',
  '- Second addition.',
  '',
  '## [0.11.0] - 2026-11-09',
  '',
  'Summary paragraph.',
  '',
  '### Added',
  '',
  '- Released addition.',
  '',
].join('\n');

test('unreleased_groups_in_file_order', () => {
  // Fixed comes before Added in the file, so it comes first here.
  assert.deepEqual(parseUnreleased(CHANGELOG, '0.11.0'), [
    { group: 'Fixed', entries: ['First fix.'] },
    { group: 'Added', entries: ['First addition.', 'Second addition.'] },
  ]);
});

test('unreleased_missing_heading_throws', () => {
  const text = CHANGELOG.replace('## [Unreleased]', '## Next');
  assert.throws(() => parseUnreleased(text, '0.11.0'), /no `## \[Unreleased\]`/);
});

test('unreleased_empty_section_is_empty', () => {
  const text = ['## [Unreleased]', '', '## [0.11.0] - 2026-11-09', '', '### Added', '', '- Old.', ''].join('\n');
  assert.deepEqual(parseUnreleased(text, '0.11.0'), []);
});

test('entry_continuation_lines_join', () => {
  const text = [
    '## [Unreleased]',
    '',
    '### Added',
    '',
    '- A long entry',
    '  wrapped over',
    '  three lines.',
    '- Next.',
    '',
    '## [0.11.0] - 2026-11-09',
    '',
  ].join('\n');
  assert.deepEqual(parseUnreleased(text, '0.11.0'), [
    { group: 'Added', entries: ['A long entry wrapped over three lines.', 'Next.'] },
  ]);
});

// The release commit lands before its tag: 0.12.0 is written down, 0.11.0 is the tag.
const AHEAD = CHANGELOG.replace(
  '## [0.11.0] - 2026-11-09',
  ['## [0.12.0] - 2026-11-12', '', '### Changed', '', '- Next release change.', '', '### Added', '', '- Next release addition.', '', '## [0.11.0] - 2026-11-09'].join('\n'),
);

test('release_section_for_tag_not_top', () => {
  assert.deepEqual(releaseSection(AHEAD, '0.11.0'), { version: '0.11.0', date: '2026-11-09' });
});

test('tag_without_section_throws', () => {
  assert.throws(() => releaseSection(CHANGELOG, '0.12.0'), /no `## \[0\.12\.0\] - YYYY-MM-DD`/);
  assert.throws(() => parseUnreleased(CHANGELOG, '0.12.0'), /no `## \[0\.12\.0\] - YYYY-MM-DD`/);
});

test('sections_above_tag_count_as_unreleased', () => {
  // The untagged 0.12.0 section joins Unreleased, groups merged in file order.
  assert.deepEqual(parseUnreleased(AHEAD, '0.11.0'), [
    { group: 'Fixed', entries: ['First fix.'] },
    { group: 'Added', entries: ['First addition.', 'Second addition.', 'Next release addition.'] },
    { group: 'Changed', entries: ['Next release change.'] },
  ]);
});

test('cadence_over_days', () => {
  const verdict = cadence(8, 10, LIMITS);
  assert.equal(verdict.verdict, CadenceVerdict.OverDays);
  assert.equal(verdict.over, true);
  assert.match(verdict.reason, /8 days since the release; the limit is 7/);
});

test('cadence_over_commits', () => {
  const verdict = cadence(2, 201, LIMITS);
  assert.equal(verdict.verdict, CadenceVerdict.OverCommits);
  assert.equal(verdict.over, true);
  assert.match(verdict.reason, /201 commits since the release; the limit is 200/);
});

test('cadence_at_limit_is_within', () => {
  const verdict = cadence(7, 200, LIMITS);
  assert.equal(verdict.verdict, CadenceVerdict.Within);
  assert.equal(verdict.over, false);
});

test('entry_outside_group_throws', () => {
  const text = ['## [Unreleased]', '', '- No group.', '', '## [0.11.0] - 2026-11-09', ''].join('\n');
  assert.throws(() => parseUnreleased(text, '0.11.0'), /entry outside a `###` group/);
});
