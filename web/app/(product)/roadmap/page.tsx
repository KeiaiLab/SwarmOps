// /roadmap, from web/content/roadmap.md.
import type { Metadata } from 'next';
import { MarkdownPage } from '@/components/markdown-page';
import { roadmapPage } from '@/lib/source';

export const metadata: Metadata = {
  title: roadmapPage.title,
  description: roadmapPage.description,
  alternates: { canonical: '/roadmap' },
};

export default function Page() {
  return <MarkdownPage title={roadmapPage.title} description={roadmapPage.description} body={roadmapPage.body} />;
}
