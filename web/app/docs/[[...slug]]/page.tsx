// One docs page: the repository Markdown as GitHub shows it (its own `# ` title included), with
// breadcrumbs, the table of contents and a link to edit the file on GitHub.
import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { DocsBody, DocsPage, EditOnGitHub } from 'fumadocs-ui/layouts/docs/page';
import { getMDXComponents } from '@/components/mdx';
import { DocsLanding } from '@/components/docs-landing';
import { githubUrl } from '@/lib/docs-paths';
import { nodeText } from '@/lib/node-text';
import { repoPath, source } from '@/lib/source';

// The page title is the Markdown's own `# ` heading; the TOC lists the sections below it.
const TOC_MIN_DEPTH = 2;

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

  const MDX = page.data.body;
  const toc = page.data.toc
    .filter((item) => item.depth >= TOC_MIN_DEPTH)
    .map((item) => ({ ...item, title: nodeText(item.title) }));

  return (
    <DocsPage toc={toc} breadcrumb={{ includeRoot: true, includePage: true }}>
      <DocsBody>
        <MDX components={getMDXComponents()} />
      </DocsBody>
      <EditOnGitHub href={githubUrl(repoPath(page.data))} />
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
    alternates: { canonical: page.url },
  };
}
