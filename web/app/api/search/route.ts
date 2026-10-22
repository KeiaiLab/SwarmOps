// The search index, exported as a static file at build time: the dialog downloads it once and
// searches in the browser (Orama), so the site needs no search server.
import { createFromSource } from 'fumadocs-core/search/server';
import { source } from '@/lib/source';

export const dynamic = 'force-static';
export const revalidate = false;

export const { staticGET: GET } = createFromSource(source, { language: 'english' });
