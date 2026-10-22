// "View as Markdown": a docs page's link to its source file, served beside the page.
//
//   /docs/guides/install  ->  /docs/guides/install.md   (a copy of docs/guides/install.md)
//   /docs                 ->  /docs.md                  (a copy of docs/README.md)
//
// scripts/markdown-twins.mjs reads `data-markdown-source` from each built page and copies that
// repository file to the link's address, so the link and the file it names come from one place.
import { FileText } from 'lucide-react';
import { buttonVariants } from '@/components/ui/button';
import { cn } from '@/lib/cn';

const MARKDOWN_EXT = '.md';

export interface ViewMarkdownProps {
  // The page's URL, `/docs/guides/install`.
  url: string;
  // The page's repository file, `docs/guides/install.md`.
  source: string;
}

export function ViewMarkdown({ url, source }: ViewMarkdownProps) {
  // A plain link, no client routing: the target is a text file, not a page.
  return (
    <a
      href={url + MARKDOWN_EXT}
      data-markdown-source={source}
      type="text/markdown"
      className={cn(buttonVariants({ variant: 'secondary', size: 'sm' }), 'not-prose self-start')}
    >
      <FileText className="size-3.5" />
      View as Markdown
    </a>
  );
}
