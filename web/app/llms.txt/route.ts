// /llms.txt (llmstxt.org): the site's name and tagline, then the docs pages by sidebar group,
// with absolute links so the file reads the same wherever it is fetched from.
import { llms } from 'fumadocs-core/source';
import { SITE_NAME, SITE_TAGLINE, SITE_URL } from '@/lib/site';
import { source } from '@/lib/source';

export const dynamic = 'force-static';
export const revalidate = false;

export function GET() {
  // One H1 per file: the docs index comes as a section under the site's name.
  const index = llms(source)
    .index()
    .replace(/^# /, '## ')
    .replaceAll('](/', `](${SITE_URL}/`);
  const body = `# ${SITE_NAME}\n\n> ${SITE_TAGLINE}\n\n${index}`;

  return new Response(body, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
