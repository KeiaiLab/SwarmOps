// One docs page: the repository Markdown as GitHub shows it (its own `# ` title included), framed
// as the design's docs chrome:
//
//   Docs > Guides > Commit and land
//   [GUIDE]
//   # Commit and land
//   Describes v0.11.0 · Last changed 2026-11-07 · View as Markdown · Copy Markdown
//   (a file changed after the tag: Describes stable at 3a561c91, 2026-11-12; latest release
//   v0.11.0 · what changed · ...)
//   ...page...
//   Edit on GitHub · Report a problem with this page
//   < Previous                                   Next >
import type { ComponentProps } from 'react';
import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { DocsBody, DocsPage } from 'fumadocs-ui/layouts/docs/page';
import { getMDXComponents } from '@/components/mdx';
import { DocsLanding } from '@/components/docs-landing';
import { KindBadge } from '@/components/docs/kind-badge';
import { PageActions } from '@/components/docs/page-actions';
import { PageMeta } from '@/components/docs/page-meta';
import { headings } from '@/components/heading';
import { ReferenceBody } from '@/components/reference/reference-body';
import { twinUrl } from '@/components/view-markdown';
import { githubUrl, pageKindOf } from '@/lib/docs-paths';
import { lastChanged } from '@/lib/docs-meta';
import { nodeText } from '@/lib/node-text';
import { pageKind } from '@/lib/reference/pages';
import { LATEST_RELEASE } from '@/lib/release';
import { repoPath, source } from '@/lib/source';
import { fileAhead } from '@/lib/stable';

// The page title is the Markdown's own `# ` heading; the TOC lists the sections below it, and
// only on a page with enough of them to need one.
const TOC_MIN_DEPTH = 2;
const SECTION_DEPTH = 2;
const TOC_MIN_SECTIONS = 3;

// The twin's media type, for the page's `<link rel="alternate">`.
const MARKDOWN_TYPE = 'text/markdown';

interface Props {
  params: Promise<{ slug?: string[] }>;
}

export default async function Page({ params }: Props) {
  const { slug } = await params;
  const page = source.getPage(slug);

  // The docs index: docs/README.md when there is one, else a generated landing page.
  if (!page) {
    if (slug === undefined || slug.length === 0) {
      return <DocsLanding />;
    }
    notFound();
  }

  // 1. The page's facts: file, kind, what it describes (release, or stable when changed since), contents.
  const file = repoPath(page.data);
  const MDX = page.data.body;
  const kind = pageKind(file);
  const meta = (
    <PageMeta
      key="page-meta"
      url={page.url}
      source={file}
      sourceUrl={githubUrl(file)}
      lastChanged={lastChanged(file)}
      describes={fileAhead(file)}
    />
  );
  const toc = page.data.toc
    .filter((item) => item.depth >= TOC_MIN_DEPTH)
    .map((item) => ({ ...item, title: nodeText(item.title) }));
  const sections = toc.filter((item) => item.depth === SECTION_DEPTH).length;

  // 2. The meta line goes under the first `# `, the page's title.
  let titled = false;
  function Title(props: ComponentProps<'h1'>) {
    if (titled) {
      return <headings.h1 {...props} />;
    }
    titled = true;
    return (
      <>
        <headings.h1 {...props} />
        {meta}
      </>
    );
  }

  return (
    <DocsPage
      toc={toc}
      tableOfContent={{ enabled: sections >= TOC_MIN_SECTIONS }}
      tableOfContentPopover={{ enabled: sections >= TOC_MIN_SECTIONS }}
      breadcrumb={{ includeRoot: true, includePage: true }}
      footer={{ enabled: true }}
    >
      <KindBadge kind={pageKindOf(file)} />
      <DocsBody>
        {kind ? (
          <ReferenceBody page={page} kind={kind} afterTitle={meta} />
        ) : (
          <MDX components={getMDXComponents({ h1: Title })} />
        )}
      </DocsBody>
      <PageActions source={file} />
    </DocsPage>
  );
}

export function generateStaticParams() {
  const params = source
    .getPages()
    .map((page) => ({ slug: page.slugs }));

  // The landing page renders even when docs/README.md does not exist.
  if (!params.some((p) => p.slug.length === 0)) {
    params.push({ slug: [] });
  }
  return params;
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { slug } = await params;
  const page = source.getPage(slug);
  if (!page) {
    return { title: 'Documentation', alternates: { canonical: '/docs' } };
  }
  return {
    title: page.data.title,
    description: page.data.description,
    alternates: { canonical: page.url, types: { [MARKDOWN_TYPE]: twinUrl(page.url) } },
    other: { 'swarmops:release': LATEST_RELEASE.version },
  };
}
