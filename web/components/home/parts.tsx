// The home page's shared parts: a numbered section, inline code, links, the primary action and
// a renderer for the inline Markdown of a docs/goals.md cell. Server components only.
import { Fragment, type ReactNode } from 'react';
import * as path from 'node:path';
import { cn } from '@/lib/cn';
import { docUrl, githubUrl, isDocPage } from '@/lib/docs-paths';
import { QUICK_START_URL } from '@/lib/site';

export const H2 = 'font-serif text-2xl font-normal tracking-tight text-foreground';
export const H3 = 'text-base font-semibold text-foreground';
export const PARA = 'max-w-(--measure)';
export const LINK =
  'text-primary underline decoration-1 underline-offset-4 decoration-primary/55 hover:decoration-2 hover:decoration-primary';
export const LINK_44 = cn(LINK, 'inline-flex min-h-11 items-center');

/** Inline code outside the prose stylesheet: free to break inside a narrow cell. */
export function C({ children }: { children: ReactNode }) {
  return (
    <code className="rounded-sm bg-muted px-1 font-mono text-foreground [overflow-wrap:anywhere]">
      {children}
    </code>
  );
}

export function Arrow() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 14 14"
      aria-hidden="true"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M2 7h10M8 3l4 4-4 4" />
    </svg>
  );
}

/** The one primary action, at the top and again at the end: same text, same address. */
export function PrimaryAction({ className }: { className?: string }) {
  return (
    <a
      data-primary
      href={QUICK_START_URL}
      className={cn(
        'inline-flex min-h-12 items-center justify-center gap-2 rounded-lg bg-primary px-6 text-base font-semibold text-primary-foreground hover:underline',
        className,
      )}
    >
      Quick start
      <Arrow />
    </a>
  );
}

/**
 * A section of the ledger: a hairline, then its row label (`01 First commands`) in mono caps.
 *
 *   ───────────────────────────────
 *   01  FIRST COMMANDS
 *   ## heading ...
 */
export function Section({ id, n, label, children }: { id: string; n: number; label: string; children: ReactNode }) {
  return (
    <section id={id} aria-labelledby={`${id}-title`} className="section scroll-mt-14 border-t border-border">
      <div className="page">
        <p className="caption mb-4 flex gap-3 text-muted-foreground">
          <span className="text-foreground">{String(n).padStart(2, '0')}</span>
          {label}
        </p>
        {children}
      </div>
    </section>
  );
}

// docs/goals.md links are relative to docs/: `operate/serve-differences.md` is a docs page.
const DOCS_DIR = 'docs';

function cellHref(href: string): string {
  // 1. Absolute addresses and anchors stay as written.
  if (/^[a-z]+:/i.test(href) || href.startsWith('#') || href.startsWith('/')) {
    return href;
  }

  // 2. A repository path: its docs page, or its page on GitHub.
  const [file, hash] = href.split('#');
  const repoPath = path.posix.normalize(`${DOCS_DIR}/${file}`);
  const url = isDocPage(repoPath) ? docUrl(repoPath) : githubUrl(repoPath);
  return hash ? `${url}#${hash}` : url;
}

// Code spans, links and bold: all a goals cell uses. Anything else prints as written.
const INLINE = /`([^`]+)`|\[([^\]]+)\]\(([^)\s]+)\)|\*\*([^*]+)\*\*/g;

/** One cell of docs/goals.md, rendered whole. */
export function InlineMarkdown({ text }: { text: string }) {
  const parts: ReactNode[] = [];
  let at = 0;

  for (const match of text.matchAll(INLINE)) {
    const [whole, code, linkText, href, bold] = match;
    parts.push(text.slice(at, match.index));
    at = match.index + whole.length;

    // One mark per match: code, a link (its text may hold code) or bold.
    if (code !== undefined) {
      parts.push(<C key={at}>{code}</C>);
      continue;
    }
    if (linkText !== undefined) {
      parts.push(
        <a key={at} className={LINK} href={cellHref(href)}>
          <InlineMarkdown text={linkText} />
        </a>,
      );
      continue;
    }
    parts.push(<strong key={at}>{bold}</strong>);
  }
  parts.push(text.slice(at));

  return (
    <>
      {parts.map((part, i) => (
        <Fragment key={i}>{part}</Fragment>
      ))}
    </>
  );
}
