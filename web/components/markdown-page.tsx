// A product page or the roadmap, from web/content/: its front matter title and description over
// its Markdown, as the readable-site design board's Product overview board lays it out:
//
//   [ Overview | Features | How it works | SwarmOps and git ]    product pages only
//   title (the page's one h1), description
//   +------------------------------+  +-------------------+
//   | the Markdown                 |  | At a glance       |
//   | (its own `# ` line dropped)  |  | Pre-1.0           |
//   |                              |  | On this page      |
//   +------------------------------+  +-------------------+  the rail sticks; below it at 1023
//
// Plain HTML elements, no client components, so these pages ship only the framework's script.
import type { ComponentProps } from 'react';
import type { TOCItemType } from 'fumadocs-core/toc';
import type { MDXComponents, MDXContent } from 'mdx/types';
import { cn } from '@/lib/cn';
import { LATEST_RELEASE } from '@/lib/release';
import { CHANGELOG_URL } from '@/lib/site';
import { productSource, roadmapPage } from '@/lib/source';

interface Props {
  title: string;
  description: string;
  body: MDXContent;
}

/** The product section's pages, in the order of their tab strip. */
const PRODUCT_TABS = [
  { text: 'Overview', url: '/product' },
  { text: 'Features', url: '/product/features' },
  { text: 'How it works', url: '/product/how-it-works' },
  { text: 'SwarmOps and git', url: '/product/vs-git' },
];

const VERSIONING_URL = '/docs/policies/versioning-and-stability';

// The rail lists the page's sections, not its subsections.
const RAIL_DEPTH = 2;

// The label strip's own type is caps (global.css); the hint beside it reads as a sentence.
const SELECT_HINT = 'One click selects it';

const CAP = 'text-[13px]/5 font-semibold uppercase tracking-[0.08em] text-muted-foreground';
const LINK =
  'text-primary underline decoration-1 underline-offset-[0.22em] decoration-[color-mix(in_srgb,var(--primary)_55%,transparent)] hover:decoration-2 hover:decoration-primary';
const CODE = 'rounded-[4px] bg-muted px-[0.32em] py-[0.1em] font-mono text-[calc(1em-2px)] text-foreground';

/** Where a page lives and its sections: the product page or the roadmap of that title. */
interface PageInfo {
  url: string | null;
  toc: TOCItemType[];
}

function pageInfo(title: string): PageInfo {
  // 1. A product page, found by its front matter title (the callers pass only that).
  const page = productSource
    .getPages()
    .find((p) => p.data.title === title);
  if (page) {
    return { url: page.url, toc: page.data.toc };
  }

  // 2. The roadmap, which has no tab of its own.
  if (roadmapPage.title === title) {
    return { url: null, toc: roadmapPage.toc };
  }
  return { url: null, toc: [] };
}

/**
 * A code figure without a copy button, so without script: the label and a hint in its strip
 * (global.css, "Code figures"), the code below, selected whole by one click.
 */
export function SelectFigure({ label, className, children, ...props }: ComponentProps<'pre'> & { label: string }) {
  return (
    <figure className="code-block not-prose">
      <figcaption className="flex min-h-9 items-center justify-between gap-4 border-b border-border px-4 max-sm:min-h-11">
        <span>{label}</span>
        <span className="text-[14px]/5 font-normal normal-case tracking-normal">{SELECT_HINT}</span>
      </figcaption>
      <pre
        {...props}
        className={cn(
          className,
          'm-0 overflow-x-auto px-[18px] pt-3.5 pb-4 font-mono text-[14px]/6 text-[var(--text)] [user-select:all]',
        )}
      >
        {children}
      </pre>
    </figure>
  );
}

// A code block from source.config.ts (a bare <pre class="code-block" data-label>) as a select
// figure; its title would only repeat the label as a tooltip.
function SelectableBlock({ className, children, title, ...props }: ComponentProps<'pre'>) {
  const classes = (className ?? '').split(/\s+/);
  const label = (props as Record<string, unknown>)['data-label'];
  if (!classes.includes('code-block') || typeof label !== 'string') {
    return (
      <pre className={className} title={title} {...props}>
        {children}
      </pre>
    );
  }

  return (
    <SelectFigure label={label} className={cn(classes.filter((name) => name !== 'code-block'))} {...props}>
      {children}
    </SelectFigure>
  );
}

// The Markdown's elements: the front matter title is the page's one h1, a wide table scrolls
// inside its column, and sections clear the sticky header when a link jumps to them.
const BODY_COMPONENTS: MDXComponents = {
  h1: () => null,
  h2: ({ className, ...props }) => <h2 className={cn('scroll-mt-20', className)} {...props} />,
  h3: ({ className, ...props }) => <h3 className={cn('scroll-mt-20', className)} {...props} />,
  table: (props) => (
    <div className="overflow-x-auto">
      <table {...props} />
    </div>
  ),
  pre: SelectableBlock,
};

/** The facts beside the home page's hero and every product page, with the Pre-1.0 note. */
export function AtAGlance({ className }: { className?: string }) {
  const { version, date, preRelease } = LATEST_RELEASE;
  const facts = [
    { term: 'Runs on', detail: <>Linux, x86_64 or aarch64, glibc 2.34 or newer (Ubuntu 22.04, Debian 12, RHEL 9)</> },
    { term: 'Shape', detail: <>One Rust program, no external database, no daemon</> },
    {
      term: 'Interfaces',
      detail: (
        <>
          The <code className={CODE}>swarmops</code> and <code className={CODE}>so</code> commands; an MCP server
          with 38 tools
        </>
      ),
    },
    {
      term: 'Latest release',
      detail: (
        <>
          v{version}, {date} ·{' '}
          <a className={LINK} href={CHANGELOG_URL}>
            Changelog
          </a>
        </>
      ),
    },
    { term: 'Licence', detail: <>MIT</> },
  ];

  return (
    <aside aria-label="At a glance" className={cn('self-start border-t-2 border-foreground pt-3.5', className)}>
      {/* 1. The facts, one row each. */}
      <p className={CAP}>At a glance</p>
      <dl className="mt-2">
        {facts.map((fact) => (
          <div key={fact.term} className="border-b border-border py-3">
            <dt className="text-[14px]/5 font-semibold text-muted-foreground">{fact.term}</dt>
            <dd className="mt-0.5 text-[16px]/[26px] text-[var(--text)]">{fact.detail}</dd>
          </div>
        ))}
      </dl>

      {/* 2. Below 1.0 a minor release may still break the contract; the reader is told so. */}
      {preRelease && (
        <p className="mt-4 rounded-r-lg border-l-2 border-ring bg-card px-4 py-3 text-[15px]/6 text-[var(--text)]">
          <strong className="block font-semibold text-foreground">Pre-1.0</strong>A minor release may still break a
          command or its output (
          <a className={LINK} href={VERSIONING_URL}>
            versioning policy
          </a>
          ).
        </p>
      )}
    </aside>
  );
}

function ProductTabs({ current }: { current: string }) {
  return (
    <nav aria-label="Product" className="border-b border-border">
      <div className="mx-auto w-full max-w-[1104px] px-8 max-sm:px-5">
        <ul className="flex gap-2 max-sm:grid max-sm:grid-cols-2 max-sm:gap-0">
          {PRODUCT_TABS.map((tab) => (
            <li key={tab.url} className="sm:first:[&>a]:pl-0">
              <a
                href={tab.url}
                aria-current={tab.url === current ? 'page' : undefined}
                className="relative -mb-px inline-flex min-h-12 items-center px-3 text-[15px] text-[var(--text)] no-underline max-sm:flex"
              >
                {tab.text}
              </a>
            </li>
          ))}
        </ul>
      </div>
    </nav>
  );
}

function OnThisPage({ toc }: { toc: TOCItemType[] }) {
  const sections = toc.filter((item) => item.depth === RAIL_DEPTH);
  if (sections.length === 0) {
    return null;
  }

  return (
    <nav aria-label="On this page" className="mt-10 pt-1.5">
      <p className={cn(CAP, 'mb-2.5')}>On this page</p>
      <ol className="list-none p-0">
        {sections.map((item) => (
          <li key={item.url}>
            <a
              href={item.url}
              className="block border-l border-border py-1 pl-3.5 text-[14px]/[22px] text-muted-foreground no-underline hover:text-foreground"
            >
              {item.title}
            </a>
          </li>
        ))}
      </ol>
    </nav>
  );
}

export function MarkdownPage({ title, description, body: Body }: Props) {
  const { url, toc } = pageInfo(title);

  return (
    <>
      {/* 1. The product section's tabs, the current page marked. */}
      {url && <ProductTabs current={url} />}

      <main
        id="main"
        className="mx-auto grid w-full max-w-[1104px] grid-cols-1 px-8 pt-14 pb-24 max-sm:px-5 max-sm:pt-8 max-sm:pb-16 lg:grid-cols-[minmax(0,1fr)_300px] lg:gap-x-20"
      >
        {/* 2. The page's one h1 and its description, across both columns. */}
        <div className="mb-10 lg:col-span-2">
          <h1 className="mb-4 font-serif text-[42px]/[50px] font-normal tracking-[-0.01em] text-foreground max-sm:text-[32px]/[40px]">
            {title}
          </h1>
          <p className="max-w-[60ch] text-[20px]/8 text-muted-foreground max-sm:text-[18px]/[30px]">{description}</p>
        </div>

        {/* 3. The Markdown. */}
        <div className="prose min-w-0">
          <Body components={BODY_COMPONENTS} />
        </div>

        {/* 4. The fact rail, beside the text and sticky on wide screens, after it on narrow ones. */}
        <div className="max-lg:mt-16">
          <div className="lg:sticky lg:top-20">
            <AtAGlance />
            <OnThisPage toc={toc} />
          </div>
        </div>
      </main>
    </>
  );
}
