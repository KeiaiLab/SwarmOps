// A product page from web/content/: its front matter title and description over its Markdown.
// Plain HTML elements, no client components, so these pages ship only the framework's script.
import type { MDXContent } from 'mdx/types';

interface Props {
  title: string;
  description: string;
  body: MDXContent;
}

export function MarkdownPage({ title, description, body: Body }: Props) {
  return (
    <main id="main" className="mx-auto w-full max-w-3xl px-4 py-12">
      <h1 className="mb-3 text-4xl font-bold tracking-tight">{title}</h1>
      <p className="mb-10 text-lg text-fd-muted-foreground">{description}</p>
      <div className="prose">
        <Body />
      </div>
    </main>
  );
}
