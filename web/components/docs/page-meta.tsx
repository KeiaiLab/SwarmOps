// The line under a docs page's title: what it describes, when its file last changed, and the page
// as Markdown. A file untouched since the tag describes the release; one changed after it
// describes stable at its last change:
//
//   Describes v0.11.0 · Last changed 2026-11-07 · View as Markdown · [Copy Markdown]
//   Describes stable at 3a561c91, 2026-11-12; latest release v0.11.0 · what changed · ...
//
// scripts/markdown-twins.mjs reads `data-last-changed` and `data-source-url` from this line for
// the twin's header, so the page and its twin state the same facts.
import { MarkdownCopyButton } from 'fumadocs-ui/layouts/docs/page';
import { ViewMarkdown, twinUrl } from '@/components/view-markdown';
import { REPO_URL } from '@/lib/docs-paths';
import { LATEST_RELEASE } from '@/lib/release';
import type { FileAhead } from '@/lib/stable';

// Where "what changed" points: the commits and changes since the release.
const NOW_URL = '/now';

const LINK_CLASS = 'text-fd-primary underline-offset-4 transition-colors duration-120 hover:underline';

export interface PageMetaProps {
  // The page's URL, `/docs/guides/install`.
  url: string;
  // The page's repository file, `docs/guides/install.md`.
  source: string;
  // Its GitHub page.
  sourceUrl: string;
  // `2026-11-07`, or '' for a file git does not track yet: no date shown.
  lastChanged: string;
  // The file's last change when it has commits after the tag, else null.
  describes: FileAhead | null;
}

function Dot() {
  return (
    <span aria-hidden="true" className="text-fd-muted-foreground">
      ·
    </span>
  );
}

/** "Describes v0.11.0", or "Describes stable at <commit>, <date>; latest release v0.11.0". */
function Describes({ ahead }: { ahead: FileAhead | null }) {
  const release = `v${LATEST_RELEASE.version}`;
  if (ahead === null) {
    return <span className="text-fd-foreground">{`Describes ${release}`}</span>;
  }

  // Ahead of the release: the file's last-change commit, linked, and where to read what changed.
  return (
    <>
      <span className="text-fd-foreground">
        Describes stable at{' '}
        <a href={`${REPO_URL}/commit/${ahead.short}`} className={LINK_CLASS}>
          {ahead.short}
        </a>
        , <time dateTime={ahead.date}>{ahead.date}</time>; latest release {release}
      </span>
      <Dot />
      <a href={NOW_URL} className={LINK_CLASS}>
        what changed
      </a>
    </>
  );
}

export function PageMeta({ url, source, sourceUrl, lastChanged, describes }: PageMetaProps) {
  return (
    <div
      data-page-meta=""
      data-last-changed={lastChanged}
      data-source-url={sourceUrl}
      className="not-prose mb-8 flex flex-wrap items-center gap-x-6 gap-y-2 border-b pb-4 font-mono text-xs tabular-nums text-fd-muted-foreground"
    >
      {/* 1. The facts: what the page describes, and its file's date. */}
      <span className="flex flex-wrap items-center gap-x-3">
        <Describes ahead={describes} />
        {lastChanged !== '' && (
          <>
            <Dot />
            <span>
              Last changed <time dateTime={lastChanged} className="text-fd-foreground">{lastChanged}</time>
            </span>
          </>
        )}
      </span>

      {/* 2. The page as Markdown: its address, and a copy of its text. */}
      <span className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <ViewMarkdown url={url} source={source} />
        <MarkdownCopyButton markdownUrl={twinUrl(url)} className="font-sans" />
      </span>
    </div>
  );
}
