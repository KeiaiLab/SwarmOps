// Exact names to pages, exported as a static JSON file at build time: the search dialog fetches it
// once and puts the page of a command, MCP tool or error code typed in full first.
//
// An MCP tool also answers to `mcp <name>`, so a tool whose name a command holds still has an
// entry: "commit" is the command's page, "mcp commit" the tool's.
import { PageKind, pageKind } from '@/lib/reference/pages';
import { lookupTable } from '@/lib/reference/lookup';
import { repoPath, source } from '@/lib/source';

export const dynamic = 'force-static';
export const revalidate = false;

const TOOL_PREFIX = 'mcp ';

/** `mcp <tool>` -> the tool's page, for every MCP tool page. */
function toolNames(): Record<string, string> {
  const names: Record<string, string> = {};
  for (const page of source.getPages()) {
    if (pageKind(repoPath(page.data)) !== PageKind.McpTool) {
      continue;
    }
    names[TOOL_PREFIX + page.data.title] = page.url;
  }
  return names;
}

export function GET() {
  return Response.json({ ...lookupTable(), ...toolNames() });
}
