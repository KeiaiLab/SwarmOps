// One command line with a copy button. The line is selectable text, so it works with scripts off;
// the site script (lib/site-script.ts) copies `data-copy` and says `Copied` in the live region.
import { Copy } from 'lucide-react';

export function CopyLine({ text, label }: { text: string; label: string }) {
  return (
    <div className="flex items-stretch rounded-lg border border-input bg-card">
      <code className="figure flex min-w-0 flex-1 items-center overflow-x-auto px-4 py-2 text-sm whitespace-nowrap text-foreground select-all">
        {text}
      </code>

      {/* The control and where it reports, side by side. */}
      <button
        type="button"
        data-copy={text}
        aria-label={`Copy: ${label}`}
        className="inline-flex min-h-11 shrink-0 items-center gap-2 border-l border-input px-4 text-sm text-muted-foreground transition-colors duration-120 hover:bg-muted hover:text-foreground [&_svg]:size-4"
      >
        <Copy aria-hidden="true" />
        Copy
      </button>
      <span aria-live="polite" className="self-center px-2 text-xs text-verdict-met empty:hidden" />
    </div>
  );
}
