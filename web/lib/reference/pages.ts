// Which docs pages are generated reference pages, and the links between them. Command pages and
// MCP tool pages link both ways through the command's `mcp_tool` metadata:
//
//   docs/reference/commands/cat.md   mcp_tool: "read_file"   ->  /docs/reference/mcp-tools/read_file
//   docs/reference/mcp-tools/read_file.md                    ->  /docs/reference/commands/cat
//
// A link is made only when its target page exists, so a tool without a page shows as a name.
import { readFileSync } from 'node:fs';
import { docUrl } from '@/lib/docs-paths';
import { repoPath, source } from '@/lib/source';
import { metaText, parseMeta, type Meta } from './meta';

const COMMANDS_DIR = 'docs/reference/commands/';
const MCP_TOOLS_DIR = 'docs/reference/mcp-tools/';
const COMMAND_INDEX = 'docs/reference/cli.md';
const ERRORS_PAGE = 'docs/reference/errors.md';
const DIR_INDEX = 'README.md';
const MARKDOWN_EXT = '.md';

export enum PageKind {
  Command = 'command',
  McpTool = 'mcp-tool',
  CommandIndex = 'command-index',
}

/** A page to link to: its title and URL. */
export interface PageLink {
  title: string;
  url: string;
}

type DocsPage = NonNullable<ReturnType<typeof source.getPage>>;

/** The kind of reference page a repository path is, or null for any other docs page. */
export function pageKind(file: string): PageKind | null {
  if (file === COMMAND_INDEX) {
    return PageKind.CommandIndex;
  }
  if (file.endsWith(`/${DIR_INDEX}`)) {
    return null;
  }
  if (file.startsWith(COMMANDS_DIR)) {
    return PageKind.Command;
  }
  if (file.startsWith(MCP_TOOLS_DIR)) {
    return PageKind.McpTool;
  }
  return null;
}

/** The page's Markdown as it is in the repository. */
export function pageMarkdown(page: DocsPage): string {
  return readFileSync(page.data.info.fullPath, 'utf8');
}

/** The URL of an error code's entry: `E_IO` -> /docs/reference/errors#e_io. */
export function errorUrl(code: string): string {
  return `${docUrl(ERRORS_PAGE)}#${code.toLowerCase()}`;
}

/** Whether a site URL is a command page's: `/docs/reference/commands/cat`. */
export function isCommandUrl(url: string): boolean {
  return url.startsWith(`${docUrl(COMMANDS_DIR + DIR_INDEX)}/`);
}

/** `docs/reference/mcp-tools/read_file.md` -> `read_file`. */
function stem(file: string): string {
  const base = file
    .split('/')
    .at(-1) ?? file;
  return base.slice(0, -MARKDOWN_EXT.length);
}

/** The cross-links of every command and MCP tool page, read once per build. */
interface CrossLinks {
  // MCP tool name -> its page.
  toolPages: Map<string, PageLink>;
  // MCP tool name -> the commands that run it.
  toolCommands: Map<string, PageLink[]>;
  // Command page URL -> its metadata.
  commandMeta: Map<string, Meta>;
}

let cached: CrossLinks | null = null;

function crossLinks(): CrossLinks {
  if (cached) {
    return cached;
  }
  const links: CrossLinks = { toolPages: new Map(), toolCommands: new Map(), commandMeta: new Map() };

  for (const page of source.getPages()) {
    const file = repoPath(page.data);
    const kind = pageKind(file);
    const link = { title: page.data.title, url: page.url };

    // 1. A tool page goes by its file name, the tool's name (`read_file` or `read-file`).
    if (kind === PageKind.McpTool) {
      links.toolPages.set(stem(file).replaceAll('-', '_'), link);
      continue;
    }
    if (kind !== PageKind.Command) {
      continue;
    }

    // 2. A command page names the tool it shares an operation with, if any.
    const meta = parseMeta(pageMarkdown(page));
    if (!meta) {
      continue;
    }
    links.commandMeta.set(page.url, meta);
    const tool = metaText(meta, 'mcp_tool');
    if (tool === '') {
      continue;
    }
    links.toolCommands.set(tool, [...(links.toolCommands.get(tool) ?? []), link]);
  }

  cached = links;
  return links;
}

/** The page of an MCP tool, or null when it has none. */
export function toolPage(tool: string): PageLink | null {
  return crossLinks().toolPages.get(tool) ?? null;
}

/** The command pages whose command runs an MCP tool. */
export function toolCommands(tool: string): PageLink[] {
  return crossLinks().toolCommands.get(tool) ?? [];
}

/** The metadata of the command page at a site URL, or null. */
export function commandMeta(url: string): Meta | null {
  return crossLinks().commandMeta.get(url) ?? null;
}
