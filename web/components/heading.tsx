// Headings with a "#" link beside the text, not around it: a heading that holds a link itself
// (the changelog's `## [0.1.0]`) would otherwise nest one <a> in another, which HTML forbids.
import type { ComponentProps } from 'react';

type Level = 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6';

function heading(Tag: Level) {
  return function Heading({ id, children, className, ...props }: ComponentProps<Level>) {
    return (
      <Tag id={id} className={`group/heading scroll-m-28 ${className ?? ''}`} {...props}>
        {children}
        {id && (
          <a
            href={`#${id}`}
            aria-label="Link to this section"
            className="ms-2 text-fd-muted-foreground no-underline opacity-0 group-hover/heading:opacity-100 focus:opacity-100"
          >
            #
          </a>
        )}
      </Tag>
    );
  };
}

export const headings = {
  h1: heading('h1'),
  h2: heading('h2'),
  h3: heading('h3'),
  h4: heading('h4'),
  h5: heading('h5'),
  h6: heading('h6'),
};
