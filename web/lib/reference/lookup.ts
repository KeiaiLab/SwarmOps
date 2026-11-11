// Exact names and their pages, for the search dialog's first hit (app/api/lookup):
//
//   "swarmops blob has", "blob has"  ->  /docs/reference/commands/blob-has
//   "read_file"                      ->  /docs/reference/mcp-tools/read_file
//   "E_BUSY"                         ->  /docs/reference/errors#e_busy
//
// A name two pages share keeps the first: commands, then MCP tools, then error codes.
import { nodeText } from '@/lib/node-text';
import { repoPath, source } from '@/lib/source';
import { PageKind, errorUrl, pageKind } from './pages';

const ERRORS_PAGE = 'docs/reference/errors.md';
const CLI_PREFIX = 'swarmops ';
const ERROR_CODE = /^E_[A-Z0-9_]+$/;

/** Every exact name, mapped to its page's URL. */
export function lookupTable(): Record<string, string> {
  const table: Record<string, string> = {};
  const add = (name: string, url: string) => {
    if (name !== '' && !(name in table)) {
      table[name] = url;
    }
  };
  const pages = source.getPages();

  // 1. Commands by their title (`swarmops blob has`) and without the program's name (`blob has`).
  for (const page of pages) {
    if (pageKind(repoPath(page.data)) !== PageKind.Command) {
      continue;
    }
    const title = page.data.title;
    add(title, page.url);
    if (title.startsWith(CLI_PREFIX)) {
      add(title.slice(CLI_PREFIX.length), page.url);
    }
  }

  // 2. MCP tools by their title, the tool's name.
  for (const page of pages) {
    if (pageKind(repoPath(page.data)) === PageKind.McpTool) {
      add(page.data.title, page.url);
    }
  }

  // 3. Error codes: the `## E_*` headings of the errors page.
  const errors = pages.find((page) => repoPath(page.data) === ERRORS_PAGE);
  if (!errors) {
    throw new Error(`lookup: ${ERRORS_PAGE} has no page`);
  }
  for (const item of errors.data.toc) {
    const code = nodeText(item.title).trim();
    if (ERROR_CODE.test(code)) {
      add(code, errorUrl(code));
    }
  }
  return table;
}
