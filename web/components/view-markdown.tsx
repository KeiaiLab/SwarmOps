// "View as Markdown": a docs page's link to its source file, served beside the page.
//
//   /docs/guides/install  ->  /docs/guides/install.md   (a copy of docs/guides/install.md)
//   /docs                 ->  /docs.md                  (a copy of docs/README.md)
//
// scripts/markdown-twins.mjs reads `data-markdown-source` from each built page and copies that
// repository file to the link's address, so the link and the file it names come from one place.
import { FileText } from 'lucide-react';

const MARKDOWN_EXT = '.md';

export interface ViewMarkdownProps {
  // The page's URL, `/docs/guides/install`.
  url: string;
  // The page's repository file, `docs/guides/install.md`.
  source: string;
}

/** The twin's address of a page URL: `/docs/guides/install` -> `/docs/guides/install.md`. */
export function twinUrl(url: string): string {
  return url + MARKDOWN_EXT;
}

export function ViewMarkdown({ url, source }: ViewMarkdownProps) {
  // A plain link, no client routing: the target is a text file, not a page.
  return (
    <a
      href={twinUrl(url)}
      data-markdown-source={source}
      type="text/markdown"
      className="inline-flex items-center gap-1 text-fd-primary underline-offset-4 transition-colors duration-120 hover:underline"
    >
      <FileText className="size-3.5" aria-hidden="true" />
      View as Markdown
    </a>
  );
}
