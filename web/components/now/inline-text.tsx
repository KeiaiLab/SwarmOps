// One CHANGELOG entry as HTML: inline.ts splits it, this renders the parts. Its links are
// relative to CHANGELOG.md, so they resolve as on the changelog page.
import { Fragment } from 'react';
import { C, LINK } from '@/components/home/parts';
import { rewriteLink } from '@/lib/docs-paths';
import { InlineKind, inlineParts, type InlinePart } from './inline';

const CHANGELOG = 'CHANGELOG.md';

const rewrite = (href: string) => rewriteLink(CHANGELOG, href);

function Part({ part }: { part: InlinePart }) {
  if (part.kind === InlineKind.Code) {
    return <C>{part.text}</C>;
  }
  if (part.kind === InlineKind.Link) {
    return (
      <a className={LINK} href={part.href}>
        <Parts parts={part.parts} />
      </a>
    );
  }
  return <>{part.text}</>;
}

function Parts({ parts }: { parts: InlinePart[] }) {
  return (
    <>
      {parts.map((part, i) => (
        <Fragment key={i}>
          <Part part={part} />
        </Fragment>
      ))}
    </>
  );
}

/** A CHANGELOG entry's text with its code spans and links. */
export function InlineText({ text }: { text: string }) {
  return <Parts parts={inlineParts(text, rewrite)} />;
}
