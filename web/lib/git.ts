// git at build time, run from the repository root:
//
//   git(['log', '-1', '--format=%cs', '--', 'docs/index.md'])   -> '2026-11-07'
//
// A shallow clone would date every page by its one commit and miss the tags, so it fails the
// build instead (requireHistory).
import { execFileSync } from 'node:child_process';
import * as path from 'node:path';

// The build runs in web/; git runs from the repository root, where the paths are relative to.
const REPO_ROOT = path.resolve('..');

const SHALLOW_ERROR = 'git: full history needed (fetch-depth: 0)';

/** git's output for `args`, trimmed, with `env` over the build's environment; throws when git exits non-zero. */
export function git(args: string[], env: Record<string, string> = {}): string {
  return execFileSync('git', args, {
    cwd: REPO_ROOT,
    encoding: 'utf8',
    env: { ...process.env, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

let historyChecked = false;

/** Fails the build in a shallow clone, once per build. */
export function requireHistory(): void {
  if (historyChecked) {
    return;
  }
  if (git(['rev-parse', '--is-shallow-repository']) === 'true') {
    throw new Error(SHALLOW_ERROR);
  }
  historyChecked = true;
}
