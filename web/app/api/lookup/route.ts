// Exact names to pages, exported as a static JSON file at build time: the search dialog fetches it
// once and puts the page of a command, MCP tool or error code typed in full first.
import { lookupTable } from '@/lib/reference/lookup';

export const dynamic = 'force-static';
export const revalidate = false;

export function GET() {
  return Response.json(lookupTable());
}
