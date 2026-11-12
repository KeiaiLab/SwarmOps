// "On stable since v<tag>" under the roadmap's Markdown: per milestone, what landed since the
// latest release, from docs/status/ (lib/status.ts):
//
//   On stable since v0.11.0                       #on-stable
//   M20
//   - 2026-11-11 · M20-4 · The upgrade command takes ... (3a561c91)
//
// Server component, no script; the roadmap's rows and dated status stay the fan-in's.
import { REPO_URL } from '@/lib/docs-paths';
import { LATEST_RELEASE } from '@/lib/release';
import { MILESTONE_STATUS } from '@/lib/status';

const SHORT_LEN = 8;

export function RoadmapStatus() {
  const tag = `v${LATEST_RELEASE.version}`;

  return (
    <section aria-labelledby="on-stable">
      {/* 1. The heading the /now page links to. */}
      <h2 id="on-stable" className="scroll-mt-20">
        On stable since {tag}
      </h2>
      {MILESTONE_STATUS.length === 0 && <p>Nothing has landed on stable since {tag}.</p>}

      {/* 2. Each milestone's landings: date, slice id, sentence, the commit on GitHub. */}
      {MILESTONE_STATUS.map(({ milestone, lines }) => (
        <div key={milestone}>
          <h3 id={`on-stable-${milestone.toLowerCase()}`} className="scroll-mt-20">
            {milestone}
          </h3>
          <ul>
            {lines.map((line) => (
              <li key={line.slice}>
                <time dateTime={line.date}>{line.date}</time> · <code>{line.slice}</code> · {line.sentence} (
                <a href={`${REPO_URL}/commit/${line.commit}`}>
                  <code>{line.commit.slice(0, SHORT_LEN)}</code>
                </a>
                )
              </li>
            ))}
          </ul>
        </div>
      ))}
    </section>
  );
}
