// The kind of a docs page as a text badge under the breadcrumb: Start, Guide, Concept, Reference,
// Operate or Design (web/docs-nav.json), so a reader knows what the page is for before reading it.
import type { DocKind } from '@/lib/docs-paths';

export function KindBadge({ kind }: { kind: DocKind }) {
  return (
    <p className="not-prose -mt-2 mb-2">
      <span className="sr-only">Page kind: </span>
      <span
        data-doc-kind={kind}
        className="inline-flex items-center rounded border border-fd-border px-2 py-0.5 font-mono text-xs uppercase tracking-wide text-fd-muted-foreground"
      >
        {kind}
      </span>
    </p>
  );
}
