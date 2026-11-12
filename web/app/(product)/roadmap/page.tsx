// /roadmap, from web/content/roadmap.md, then what landed on stable since the latest release.
import type { Metadata } from 'next';
import type { MDXProps } from 'mdx/types';
import { MarkdownPage } from '@/components/markdown-page';
import { RoadmapStatus } from '@/components/roadmap-status';
import { roadmapPage } from '@/lib/source';

export const metadata: Metadata = {
  title: roadmapPage.title,
  description: roadmapPage.description,
  alternates: { canonical: '/roadmap' },
};

// The Markdown, then the status lines in the same column.
function Body(props: MDXProps) {
  const Roadmap = roadmapPage.body;
  return (
    <>
      <Roadmap {...props} />
      <RoadmapStatus />
    </>
  );
}

export default function Page() {
  return <MarkdownPage title={roadmapPage.title} description={roadmapPage.description} body={Body} />;
}
