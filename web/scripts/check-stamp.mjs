// Checks that the built site (out/) states the commit it was built from, and what git says of
// that commit (the freshness design, section 2.3 and correction H8). Offline: out/ and git only.
//
//   stamp      out/now.json parses; its commit is HEAD (and --sha), its release the newest
//              reachable v* tag, its `since` the commits from that tag to HEAD, its ref --ref
//   pages      out/index.html and out/now.html carry data-stable-commit = HEAD
//   docs       every out/docs page has the frame line, and it reads what git says of its file:
//              "Describes stable at <commit>, <date>; latest release v<tag>" for a file with
//              commits after the tag, else "Describes v<tag>"; the changelog reads "stable at"
//              while Unreleased has entries
//   roadmap    out/roadmap.html #on-stable names the tag, and links exactly the commits of the
//              docs/status/ files the tag does not contain
//   release    out/index.html and out/product/** do not name the release before the tag, unless
//              inside an element with data-cited-version
//
//   node web/scripts/check-stamp.mjs                          over web/out, against git
//   node web/scripts/check-stamp.mjs --sha <40 hex> --ref stable   as the site workflow runs it
//   node web/scripts/check-stamp.mjs --selftest               a passing export, then one failure of each kind
//
// Exit 0 when every check passes, 1 otherwise, with one line per failure.
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';

const WEB = path.resolve(import.meta.dirname, '..');
const REPO = path.resolve(WEB, '..');
const OUT = path.join(WEB, 'out');

// The files of the export this check reads, relative to out/.
const STAMP = 'now.json';
const STAMPED_PAGES = ['index.html', 'now.html'];
const DOCS_INDEX = 'docs.html';
const DOCS_DIR = 'docs';
const CHANGELOG_PAGE = 'docs/changelog.html';
const ROADMAP = 'roadmap.html';
const HOME = 'index.html';
const PRODUCT_DIR = 'product';

// The repository's side.
const CHANGELOG = 'CHANGELOG.md';
const STATUS_ROOT = 'docs/status';
const TAG_GLOB = 'v[0-9]*';

const SHORT_LEN = 8;
const SHA_RE = /^[0-9a-f]{40}$/;
const ON_STABLE = 'on-stable';
const CITED = 'data-cited-version';

// --- git: the only place that runs it ---------------------------------------------------------

/** git's trimmed output in the repository, dates in UTC; throws when git fails. */
function git(args) {
  return execFileSync('git', args, {
    cwd: REPO,
    encoding: 'utf8',
    env: { ...process.env, TZ: 'UTC' },
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

/** git's exit status as a verdict, for the commands whose non-zero exit is an answer. */
function gitSays(args) {
  try {
    git(args);
    return true;
  } catch {
    return false;
  }
}

/** The release before `tag` (`v0.10.0`), or null when no v* tag precedes it. */
function previousTag(tag) {
  if (git(['tag', '--list', TAG_GLOB, '--merged', `${tag}^`]) === '') {
    return null;
  }
  return git(['describe', '--tags', '--abbrev=0', '--match', TAG_GLOB, `${tag}^`]);
}

/** A file's last change when it has commits after the tag, else null: lib/stable.ts `fileAhead`. */
function fileAhead(tag, repoPath) {
  if (Number(git(['rev-list', '--count', `${tag}..HEAD`, '--', repoPath])) === 0) {
    return null;
  }
  const line = git(['log', '-1', '--date=format-local:%Y-%m-%d', '--format=%H %cd', 'HEAD', '--', repoPath]);
  const [commit, date] = line.split(' ');
  return { short: commit.slice(0, SHORT_LEN), date };
}

/** The commits of the status files the tag does not contain: lib/status.ts, by its rules. */
function statusCommits(tag) {
  const commits = new Set();
  const root = path.join(REPO, STATUS_ROOT);

  for (const dir of readdirSync(root).sort()) {
    for (const file of readdirSync(path.join(root, dir)).sort()) {
      // A seed file names its commit on a second line; any other file's is the one that added it.
      const repoPath = `${STATUS_ROOT}/${dir}/${file}`;
      const seed = /^commit ([0-9a-f]{40})$/m.exec(readFileSync(path.join(REPO, repoPath), 'utf8'))?.[1];
      const added = git(['log', '--diff-filter=A', '--format=%H', 'HEAD', '--', repoPath]).split('\n')[0];
      const commit = seed ?? added;

      if (!gitSays(['merge-base', '--is-ancestor', commit, tag])) {
        commits.add(commit);
      }
    }
  }
  return commits;
}

/** Whether CHANGELOG.md's Unreleased section holds an entry. */
function unreleasedHasEntries() {
  const text = readFileSync(path.join(REPO, CHANGELOG), 'utf8');
  const section = /^## \[Unreleased\][^\n]*\n([\s\S]*?)(?=^## \[|(?![\s\S]))/m.exec(text);
  if (!section) {
    throw new Error(`${CHANGELOG}: no "## [Unreleased]" heading`);
  }
  return /^- /m.test(section[1]);
}

/** What git says of the commit checked out: the facts every check compares the export with. */
function readFacts({ sha, ref }) {
  const tag = git(['describe', '--tags', '--abbrev=0', '--match', TAG_GLOB, 'HEAD']);
  return {
    head: git(['rev-parse', 'HEAD']),
    sha,
    ref,
    tag,
    previous: previousTag(tag),
    since: Number(git(['rev-list', '--count', `${tag}..HEAD`])),
    fileAhead: (repoPath) => fileAhead(tag, repoPath),
    statusCommits: statusCommits(tag),
    unreleased: unreleasedHasEntries(),
  };
}

// --- reading the export -----------------------------------------------------------------------

/** A file of the export as text, or null when it is missing. */
function read(out, name) {
  const file = path.join(out, name);
  return existsSync(file) ? readFileSync(file, 'utf8') : null;
}

/** Every .html under `dir` of the export, as sorted paths relative to out/. */
function htmlUnder(out, dir) {
  const root = path.join(out, dir);
  if (!existsSync(root)) {
    return [];
  }
  return readdirSync(root, { recursive: true, encoding: 'utf8' })
    .filter((name) => name.endsWith('.html'))
    .map((name) => path.posix.join(dir, name.split(path.sep).join('/')))
    .sort();
}

/** Markup as the text a reader sees: no script, style, comment or tag; one space between words. */
function visible(html) {
  return html
    .replace(/<(script|style)\b[\s\S]*?<\/\1>/g, ' ')
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<[^>]*>/g, ' ')
    .replace(/\s+/g, ' ')
    .replace(/ ([,;])/g, '$1')
    .trim();
}

/** `html` from the opening tag at `start` to its matching closing tag, nested same-name tags included. */
function element(html, start) {
  const name = /^<([\w-]+)/.exec(html.slice(start))?.[1];
  const tags = new RegExp(`<(/?)${name}\\b[^>]*>`, 'g');
  tags.lastIndex = start;

  let depth = 0;
  for (let tag = tags.exec(html); tag; tag = tags.exec(html)) {
    depth += tag[1] === '/' ? -1 : 1;
    if (depth === 0) {
      return html.slice(start, tags.lastIndex);
    }
  }
  return html.slice(start);
}

/** The first element whose opening tag matches `opening` (a pattern for `<tag ...>`), or null. */
function firstElement(html, opening) {
  const found = opening.exec(html);
  return found ? element(html, found.index) : null;
}

/** `html` without the elements that carry `attribute`. */
function without(html, attribute) {
  const opening = new RegExp(`<[\\w-]+\\b[^>]*\\s${attribute}\\b[^>]*>`);
  let rest = html;
  for (let found = opening.exec(rest); found; found = opening.exec(rest)) {
    rest = rest.slice(0, found.index) + ' ' + rest.slice(found.index + element(rest, found.index).length);
  }
  return rest;
}

// --- the checks: each returns its failures as lines -------------------------------------------

/** now.json against HEAD, the commit to build, the tag, the count and the ref. */
function checkStamp(out, facts) {
  // 1. The stamp exists and is JSON.
  const text = read(out, STAMP);
  if (text === null) {
    return [`${STAMP}: missing`];
  }
  let stamp;
  try {
    stamp = JSON.parse(text);
  } catch (error) {
    return [`${STAMP}: does not parse: ${error.message}`];
  }

  // 2. Its commit is the one checked out, and the one the workflow meant to build.
  const failures = [];
  if (stamp.commit !== facts.head) {
    failures.push(`${STAMP}: commit ${stamp.commit}, HEAD is ${facts.head}`);
  }
  if (facts.sha !== null && stamp.commit !== facts.sha) {
    failures.push(`${STAMP}: commit ${stamp.commit}, the commit to build is ${facts.sha}`);
  }

  // 3. Its release, count and ref are git's.
  if (`v${stamp.release?.version}` !== facts.tag) {
    failures.push(`${STAMP}: release v${stamp.release?.version}, the newest reachable tag is ${facts.tag}`);
  }
  if (stamp.since !== facts.since) {
    failures.push(`${STAMP}: since ${stamp.since}, git counts ${facts.since} commits from ${facts.tag}`);
  }
  if (facts.ref !== null && stamp.ref !== facts.ref) {
    failures.push(`${STAMP}: ref ${stamp.ref}, expected ${facts.ref}`);
  }
  return failures;
}

/** Home and /now carry the commit in data-stable-commit. */
function checkPages(out, facts) {
  const failures = [];
  for (const page of STAMPED_PAGES) {
    const html = read(out, page);
    if (html === null) {
      failures.push(`${page}: missing`);
      continue;
    }

    const commit = /\sdata-stable-commit="([^"]*)"/.exec(html)?.[1];
    if (commit === undefined) {
      failures.push(`${page}: no data-stable-commit`);
      continue;
    }
    if (commit !== facts.head) {
      failures.push(`${page}: data-stable-commit ${commit}, HEAD is ${facts.head}`);
    }
  }
  return failures;
}

/** The frame line git expects for a docs file. */
function describes(facts, repoPath) {
  const ahead = facts.fileAhead(repoPath);
  if (ahead === null) {
    return `Describes ${facts.tag}`;
  }
  return `Describes stable at ${ahead.short}, ${ahead.date}; latest release ${facts.tag}`;
}

/** docs_frame_matches_git: every docs page's "Describes ..." line against its file's history. */
function checkDocs(out, facts) {
  const pages = [...(read(out, DOCS_INDEX) === null ? [] : [DOCS_INDEX]), ...htmlUnder(out, DOCS_DIR)];
  if (pages.length === 0) {
    return [`${DOCS_DIR}: no page in the export`];
  }

  const failures = [];
  for (const page of pages) {
    // 1. The frame line and the repository file it names.
    const meta = firstElement(read(out, page), /<div\b[^>]*\sdata-page-meta=""[^>]*>/);
    if (meta === null) {
      failures.push(`${page}: no data-page-meta`);
      continue;
    }
    const repoPath = /\sdata-source-url="[^"]*\/blob\/[^/"]+\/([^"]+)"/.exec(meta)?.[1];
    if (repoPath === undefined) {
      failures.push(`${page}: data-page-meta names no file in data-source-url`);
      continue;
    }

    // 2. What the page reads against what git says of that file.
    const got = /Describes (?:stable at .*?; latest release \S+|\S+)/.exec(visible(meta))?.[0] ?? '';
    const want = describes(facts, repoPath);
    if (got !== want) {
      failures.push(`${page}: reads "${got}", git says "${want}" of ${repoPath}`);
      continue;
    }

    // 3. The changelog is ahead of the release while Unreleased has entries.
    if (page === CHANGELOG_PAGE && facts.unreleased && !got.startsWith('Describes stable at ')) {
      failures.push(`${page}: Unreleased has entries, the page reads "${got}"`);
    }
  }
  return failures;
}

/** roadmap_status_matches_git: #on-stable names the tag and links git's set of commits. */
function checkRoadmap(out, facts) {
  // 1. The section under the roadmap's Markdown.
  const html = read(out, ROADMAP);
  if (html === null) {
    return [`${ROADMAP}: missing`];
  }
  const section = firstElement(html, new RegExp(`<section\\b[^>]*\\saria-labelledby="${ON_STABLE}"[^>]*>`));
  const heading = section && firstElement(section, new RegExp(`<h2\\b[^>]*\\sid="${ON_STABLE}"[^>]*>`));
  if (!heading) {
    return [`${ROADMAP}: no #${ON_STABLE} section`];
  }

  // 2. Its heading names the tag.
  const failures = [];
  const want = `On stable since ${facts.tag}`;
  if (visible(heading) !== want) {
    failures.push(`${ROADMAP}: #${ON_STABLE} reads "${visible(heading)}", expected "${want}"`);
  }

  // 3. Its commit links are the computed set, no more and no fewer.
  const linked = new Set([...section.matchAll(/\/commit\/([0-9a-f]{40})"/g)].map((link) => link[1]));
  for (const commit of [...facts.statusCommits].sort()) {
    if (!linked.has(commit)) {
      failures.push(`${ROADMAP}: #${ON_STABLE} lacks ${commit}, a status file's commit since ${facts.tag}`);
    }
  }
  for (const commit of [...linked].sort()) {
    if (!facts.statusCommits.has(commit)) {
      failures.push(`${ROADMAP}: #${ON_STABLE} links ${commit}, no status file's commit since ${facts.tag}`);
    }
  }
  return failures;
}

/** previous_release_typed: Home and the product pages do not name the release before the tag. */
function checkRelease(out, facts) {
  if (facts.previous === null) {
    return [];
  }

  // `v0.10.0` or `0.10.0` as a whole version: not inside 10.10.0, 0.10.01 or 0.10.0.1.
  const version = facts.previous.replace(/^v/, '').replace(/\./g, '\\.');
  const typed = new RegExp(`(?<![\\w.])v?${version}(?!\\.?\\d|\\w)`);

  const failures = [];
  for (const page of [HOME, ...htmlUnder(out, PRODUCT_DIR)]) {
    const html = read(out, page);
    if (html === null) {
      failures.push(`${page}: missing`);
      continue;
    }
    if (typed.test(visible(without(html, CITED)))) {
      failures.push(`${page}: names ${facts.previous}, the release before ${facts.tag}, outside ${CITED}`);
    }
  }
  return failures;
}

/** Every failure of the export in `out` against `facts`. */
function check(out, facts) {
  const failures = [];
  if (facts.sha !== null && facts.sha !== facts.head) {
    failures.push(`HEAD is ${facts.head}, the commit to build is ${facts.sha}`);
  }
  return [
    ...failures,
    ...checkStamp(out, facts),
    ...checkPages(out, facts),
    ...checkDocs(out, facts),
    ...checkRoadmap(out, facts),
    ...checkRelease(out, facts),
  ];
}

// --- selftest: a planted export in a temporary out/, fixed facts, no git -----------------------

const HEAD = 'a'.repeat(40);
// Stands for HEAD~1: the commit a stale export was built from.
const PARENT = 'b'.repeat(40);
const LANDED = 'c'.repeat(40);
const STRAY = 'd'.repeat(40);
const AHEAD = { short: 'e'.repeat(SHORT_LEN), date: '2026-11-12' };
const SOURCE = 'https://example.invalid/blob/stable/';

const FIXTURE_FACTS = {
  head: HEAD,
  sha: HEAD,
  ref: 'stable',
  tag: 'v0.11.0',
  previous: 'v0.10.0',
  since: 81,
  // The changelog and one guide changed after the tag; the policy did not.
  fileAhead: (repoPath) => (repoPath === 'docs/policies/clean-room.md' ? null : AHEAD),
  statusCommits: new Set([LANDED]),
  unreleased: true,
};

const BEHIND_LINE = '<span>Describes <!-- -->v0.11.0</span>';
const AHEAD_LINE =
  `<span>Describes stable at<!-- --> <a href="/commit/${AHEAD.short}">${AHEAD.short}</a>, ` +
  `<time dateTime="${AHEAD.date}">${AHEAD.date}</time>; latest release <!-- -->v0.11.0</span>`;

/** A docs page of the fixture: the frame line over `line`, for the repository file `repoPath`. */
function docsPage(repoPath, line) {
  return (
    `<html><body><div data-page-meta="" data-last-changed="2026-11-12" data-source-url="${SOURCE}${repoPath}" class="x">` +
    `<span>${line}<span>·</span><span>Last changed 2026-11-12</span></span></div><p>v0.10.0 in docs is history</p></body></html>`
  );
}

/** The files of an export every check passes, by path relative to out/. */
function fixture() {
  const stamp = { commit: HEAD, date: '2026-11-12', ref: 'stable', release: { date: '2026-11-09', version: '0.11.0' }, since: 81 };
  return {
    'now.json': `${JSON.stringify(stamp, null, 2)}\n`,
    'index.html': `<html><body><p data-stable-commit="${HEAD}" class="x">On stable</p><script>"v0.10.0"</script></body></html>`,
    'now.html': `<html><body><main data-stable-commit="${HEAD}">now</main></body></html>`,
    'docs.html': docsPage('docs/README.md', AHEAD_LINE),
    'docs/changelog.html': docsPage('CHANGELOG.md', AHEAD_LINE),
    'docs/guides/install.html': docsPage('docs/guides/install.md', AHEAD_LINE),
    'docs/policies/clean-room.html': docsPage('docs/policies/clean-room.md', BEHIND_LINE),
    'roadmap.html':
      `<html><body><section aria-labelledby="on-stable"><h2 id="on-stable" class="x">On stable since <!-- -->v0.11.0</h2>` +
      `<section><ul><li><a href="${SOURCE}../commit/${LANDED}"><code>cccccccc</code></a></li></ul></section></section>` +
      `<a href="/commit/${STRAY}">outside the section</a></body></html>`,
    'product/features.html': '<html><body><p>Since v0.11.0; version 10.10.0 and 0.10.01 are other numbers.</p></body></html>',
  };
}

/** Each case: the change to the passing fixture or its facts, and the exact failures expected. */
const CASES = [
  { name: 'matching_stamp_passes', expected: [] },
  {
    name: 'planted_stale_stamp_fails',
    files: (files) => {
      files['now.json'] = files['now.json'].replace(HEAD, PARENT);
    },
    expected: [`now.json: commit ${PARENT}, HEAD is ${HEAD}`, `now.json: commit ${PARENT}, the commit to build is ${HEAD}`],
  },
  {
    name: 'missing_stamp_fails',
    files: (files) => {
      delete files['now.json'];
    },
    expected: ['now.json: missing'],
  },
  {
    name: 'unparseable_stamp_fails',
    files: (files) => {
      files['now.json'] = '<html>';
    },
    expectedStart: ['now.json: does not parse: '],
  },
  {
    name: 'home_attribute_mismatch_fails',
    files: (files) => {
      files['index.html'] = files['index.html'].replace(HEAD, PARENT);
    },
    expected: [`index.html: data-stable-commit ${PARENT}, HEAD is ${HEAD}`],
  },
  {
    name: 'missing_attribute_fails',
    files: (files) => {
      files['now.html'] = '<html><body><main>now</main></body></html>';
    },
    expected: ['now.html: no data-stable-commit'],
  },
  {
    name: 'since_mismatch_fails',
    files: (files) => {
      files['now.json'] = files['now.json'].replace('"since": 81', '"since": 80');
    },
    expected: ['now.json: since 80, git counts 81 commits from v0.11.0'],
  },
  {
    name: 'release_not_the_tag_fails',
    files: (files) => {
      files['now.json'] = files['now.json'].replace('"version": "0.11.0"', '"version": "0.12.0"');
    },
    expected: ['now.json: release v0.12.0, the newest reachable tag is v0.11.0'],
  },
  {
    name: 'ref_mismatch_fails',
    files: (files) => {
      files['now.json'] = files['now.json'].replace('"ref": "stable"', '"ref": "v0.11.0"');
    },
    expected: ['now.json: ref v0.11.0, expected stable'],
  },
  {
    name: 'checkout_not_the_commit_to_build_fails',
    facts: { sha: PARENT },
    expected: [`HEAD is ${HEAD}, the commit to build is ${PARENT}`, `now.json: commit ${HEAD}, the commit to build is ${PARENT}`],
  },
  {
    name: 'docs_frame_mismatch_on_any_page_fails',
    files: (files) => {
      files['docs/guides/install.html'] = docsPage('docs/guides/install.md', BEHIND_LINE);
    },
    expected: [
      `docs/guides/install.html: reads "Describes v0.11.0", git says "Describes stable at ${AHEAD.short}, ${AHEAD.date}; latest release v0.11.0" of docs/guides/install.md`,
    ],
  },
  {
    name: 'docs_page_without_meta_fails',
    files: (files) => {
      files['docs/policies/clean-room.html'] = '<html><body><p>Describes v0.11.0</p></body></html>';
    },
    expected: ['docs/policies/clean-room.html: no data-page-meta'],
  },
  {
    name: 'changelog_behind_unreleased_fails',
    files: (files) => {
      files['docs/changelog.html'] = docsPage('CHANGELOG.md', BEHIND_LINE);
    },
    // git sees no commit on CHANGELOG.md after the tag, yet Unreleased has entries.
    facts: { fileAhead: (repoPath) => (repoPath === 'docs/guides/install.md' || repoPath === 'docs/README.md' ? AHEAD : null) },
    expected: ['docs/changelog.html: Unreleased has entries, the page reads "Describes v0.11.0"'],
  },
  {
    name: 'roadmap_commit_set_mismatch_fails',
    files: (files) => {
      files['roadmap.html'] = files['roadmap.html'].replace(LANDED, PARENT);
    },
    expected: [
      `roadmap.html: #on-stable lacks ${LANDED}, a status file's commit since v0.11.0`,
      `roadmap.html: #on-stable links ${PARENT}, no status file's commit since v0.11.0`,
    ],
  },
  {
    name: 'roadmap_other_tag_fails',
    files: (files) => {
      files['roadmap.html'] = files['roadmap.html'].replace('since <!-- -->v0.11.0', 'since <!-- -->v0.10.0');
    },
    expected: ['roadmap.html: #on-stable reads "On stable since v0.10.0", expected "On stable since v0.11.0"'],
  },
  {
    name: 'roadmap_without_section_fails',
    files: (files) => {
      files['roadmap.html'] = '<html><body><h2 id="on-stable">On stable since v0.11.0</h2></body></html>';
    },
    expected: ['roadmap.html: no #on-stable section'],
  },
  {
    name: 'previous_release_typed_fails',
    files: (files) => {
      files['product/features.html'] = '<html><body><p>Pre-release, at v<!-- -->0.10.0.</p></body></html>';
    },
    expected: ['product/features.html: names v0.10.0, the release before v0.11.0, outside data-cited-version'],
  },
  {
    name: 'cited_version_passes',
    files: (files) => {
      files['index.html'] += '<p>Measured at <span data-cited-version=""><span>v0.10.0</span></span>.</p>';
    },
    expected: [],
  },
];

/** Writes `files` under a new temporary out/ and returns its path. */
function plant(files) {
  const out = mkdtempSync(path.join(tmpdir(), 'check-stamp-'));
  for (const [name, text] of Object.entries(files)) {
    mkdirSync(path.dirname(path.join(out, name)), { recursive: true });
    writeFileSync(path.join(out, name), text);
  }
  return out;
}

/** Whether the failures are the expected ones: whole lines, or their starts where a case says so. */
function asExpected(got, { expected, expectedStart }) {
  const want = expected ?? expectedStart;
  const same = expected ? (line, index) => line === want[index] : (line, index) => line.startsWith(want[index]);
  return got.length === want.length && got.every(same);
}

function selftest() {
  let failed = 0;
  for (const one of CASES) {
    // 1. The passing export with this case's change, checked against this case's facts.
    const files = fixture();
    one.files?.(files);
    const out = plant(files);
    const got = check(out, { ...FIXTURE_FACTS, ...one.facts });
    rmSync(out, { recursive: true, force: true });

    // 2. The failures must be the ones the case names, message for message.
    if (asExpected(got, one)) {
      console.log(`ok   ${one.name}`);
      continue;
    }
    failed += 1;
    console.log(`FAIL ${one.name}: expected ${JSON.stringify(one.expected ?? one.expectedStart)}, got ${JSON.stringify(got)}`);
  }

  console.log(`check-stamp selftest: ${CASES.length - failed} of ${CASES.length} passed`);
  return failed === 0 ? 0 : 1;
}

// --- command line -----------------------------------------------------------------------------

/** `--sha <40 hex>` and `--ref <name>`, both optional; anything else is a usage error. */
function parseArgs(argv) {
  const options = { sha: null, ref: null, selftest: false };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === '--selftest') {
      options.selftest = true;
      continue;
    }
    if ((flag === '--sha' || flag === '--ref') && argv[index + 1]) {
      options[flag.slice(2)] = argv[index + 1];
      index += 1;
      continue;
    }
    throw new Error(`usage: check-stamp.mjs [--selftest] [--sha <40 hex>] [--ref <name>]; got ${flag}`);
  }
  if (options.sha !== null && !SHA_RE.test(options.sha)) {
    throw new Error(`--sha must be 40 hex digits, got ${options.sha}`);
  }
  return options;
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.selftest) {
    return selftest();
  }

  const failures = check(OUT, readFacts(options));
  for (const line of failures) {
    console.error(`check-stamp: ${line}`);
  }
  if (failures.length > 0) {
    return 1;
  }
  console.log('check-stamp: the export states the commit built and what git says of it');
  return 0;
}

try {
  process.exit(main());
} catch (error) {
  console.error(`check-stamp: ${error.message}`);
  process.exit(1);
}
