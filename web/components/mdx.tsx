// The components Markdown renders with: Fumadocs' defaults (code blocks with a copy button,
// tables, callouts), headings with a section link, plus any page-specific ones.
import defaultMdxComponents from 'fumadocs-ui/mdx';
import type { ComponentProps } from 'react';
import type { MDXComponents } from 'mdx/types';
import { headings } from '@/components/heading';

// A wide table scrolls inside its own box, which a keyboard can focus and scroll; the page itself
// never scrolls sideways. Code blocks already do: Fumadocs' code block is such a box.
function Table(props: ComponentProps<'table'>) {
  return (
    <div className="prose-no-margin relative my-6 overflow-x-auto" tabIndex={0} role="region" aria-label="Table">
      <table {...props} />
    </div>
  );
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    ...headings,
    table: Table,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
