// The line under a docs page's title: which release it describes, when its file last changed, and
// the page as Markdown.
//
//   Describes v0.11.0 · Last changed 2026-11-07 · View as Markdown · [Copy Markdown]
//
// scripts/markdown-twins.mjs reads `data-last-changed` and `data-source-url` from this line for
// the twin's header, so the page and its twin state the same facts.
import { MarkdownCopyButton } from 'fumadocs-ui/layouts/docs/page';
import { ViewMarkdown, twinUrl } from '@/components/view-markdown';
import { LATEST_RELEASE } from '@/lib/release';

export interface PageMetaProps {
  // The page's URL, `/docs/guides/install`.
  url: string;
  // The page's repository file, `docs/guides/install.md`.
  source: string;
  // Its GitHub page.
  sourceUrl: string;
  // `2026-11-07`, or '' for a file git does not track yet: no date shown.
  lastChanged: string;
}

function Dot() {
  return (
    <span aria-hidden="true" className="text-fd-muted-foreground">
      ·
    </span>
  );
}

export function PageMeta({ url, source, sourceUrl, lastChanged }: PageMetaProps) {
  return (
    <div
      data-page-meta=""
      data-last-changed={lastChanged}
      data-source-url={sourceUrl}
      className="not-prose mb-8 flex flex-wrap items-center gap-x-6 gap-y-2 border-b pb-4 font-mono text-xs tabular-nums text-fd-muted-foreground"
    >
      {/* 1. The facts: release and date. */}
      <span className="flex flex-wrap items-center gap-x-3">
        <span className="text-fd-foreground">{`Describes v${LATEST_RELEASE.version}`}</span>
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
