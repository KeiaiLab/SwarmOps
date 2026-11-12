// /now.json: the stamp the site gate and the hourly probe read, keys sorted, no build time, so two
// builds of one commit write the same file:
//
//   {"cadence":{...},"commit":"<40 hex>","date":"2026-11-12","ref":"stable",
//    "release":{"date":"2026-11-09","version":"0.11.0"},"since":81}
import { LATEST_RELEASE } from '@/lib/release';
import { STABLE } from '@/lib/stable';

export const dynamic = 'force-static';
export const revalidate = false;

/** `value` with every object's keys in sorted order. */
function sorted(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(sorted);
  }
  if (value === null || typeof value !== 'object') {
    return value;
  }
  const entries = Object.entries(value).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return Object.fromEntries(entries.map(([key, inner]) => [key, sorted(inner)]));
}

export function GET() {
  const stamp = {
    commit: STABLE.commit,
    date: STABLE.date,
    ref: STABLE.ref,
    release: { version: LATEST_RELEASE.version, date: LATEST_RELEASE.date },
    since: STABLE.since,
    cadence: STABLE.cadence,
  };

  return new Response(`${JSON.stringify(sorted(stamp), null, 2)}\n`, {
    headers: { 'Content-Type': 'application/json; charset=utf-8' },
  });
}
