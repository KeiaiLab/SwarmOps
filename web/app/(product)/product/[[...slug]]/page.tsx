// /product and its sub-pages, one per Markdown file under web/content/product/.
import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { MarkdownPage } from '@/components/markdown-page';
import { productSource } from '@/lib/source';

interface Props {
  params: Promise<{ slug?: string[] }>;
}

export default async function Page({ params }: Props) {
  const { slug } = await params;
  const page = productSource.getPage(slug);
  if (!page) {
    notFound();
  }
  return <MarkdownPage title={page.data.title} description={page.data.description} body={page.data.body} />;
}

export function generateStaticParams() {
  return productSource.generateParams();
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { slug } = await params;
  const page = productSource.getPage(slug);
  if (!page) {
    notFound();
  }
  return {
    title: page.data.title,
    description: page.data.description,
    alternates: { canonical: page.url },
  };
}
