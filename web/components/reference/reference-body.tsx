// The body of a generated reference page. The Markdown stays what GitHub shows; the site renders
// some of its parts with components instead:
//
//   yaml block under the title           -> MetaCard
//   ## Required / Optional arguments     -> one Accordion entry per `### <arg>`
//   ## Examples                          -> Tabs, one per `### CLI` / `### MCP`
//   ## Errors                            -> each code a Badge linking to its entry
//   docs/reference/cli.md command lists  -> CommandIndex, one table with a filter
import { Fragment, isValidElement, type ComponentProps, type ReactNode } from 'react';
import Link from 'next/link';
import defaultMdxComponents from 'fumadocs-ui/mdx';
import { headings } from '@/components/heading';
import { getMDXComponents } from '@/components/mdx';
import { Badge } from '@/components/ui/badge';
import { nodeText } from '@/lib/node-text';
import { parseMeta, metaText, type Meta } from '@/lib/reference/meta';
import {
  PageKind,
  commandMeta,
  isCommandUrl,
  pageMarkdown,
  toolCommands,
  toolPage,
} from '@/lib/reference/pages';
import type { source } from '@/lib/source';
import type { IndexCommand, IndexFamily, IndexGroup } from './command-index';
import { ArgumentAccordion, CommandIndex, ExampleTabs } from './lazy';
import { MetaCard } from './meta-card';
import { blocksOf, asLink, childrenOf, cut, isA, type Block, type Part } from './tree';

type DocsPage = NonNullable<ReturnType<typeof source.getPage>>;

// The generated pages' section titles (the generator's contract, not translated).
const ARGUMENT_SECTIONS = ['Required arguments', 'Optional arguments'];
const EXAMPLES_SECTION = 'Examples';
const ERRORS_SECTION = 'Errors';
const SUMMARY_SEPARATOR = /^:\s*/;
const COMMANDS_ID = 'commands';

// The site's Markdown components, and markers for the blocks this module replaces: each renders
// as the site's own, and its identity tells the block apart in the rendered tree.
const base = getMDXComponents();
const BasePre = defaultMdxComponents.pre;

function H2(props: ComponentProps<'h2'>) {
  return <headings.h2 {...props} />;
}

function H3(props: ComponentProps<'h3'>) {
  return <headings.h3 {...props} />;
}

function Pre(props: ComponentProps<'pre'>) {
  return <BasePre {...props} />;
}

function Ul(props: ComponentProps<'ul'>) {
  return <ul {...props} />;
}

const markers = { ...base, h2: H2, h3: H3, pre: Pre, ul: Ul };

/** `### --message` subsections as Accordion entries, all open, so the page reads in full. */
function argumentList(body: Block[]): ReactNode {
  const { lead, parts } = cut(body, H3);
  if (parts.length === 0) {
    return body;
  }
  const entries = parts.map((part) => ({ id: part.id, label: part.heading.props.children, content: part.body }));
  return (
    <>
      {lead}
      <ArgumentAccordion entries={entries} />
    </>
  );
}

/** `### CLI` and `### MCP` subsections as Tabs. */
function exampleTabs(body: Block[]): ReactNode {
  const { lead, parts } = cut(body, H3);
  if (parts.length === 0) {
    return body;
  }
  const tabs = parts.map((part) => ({ id: part.id, label: part.heading.props.children, content: part.body }));
  return (
    <>
      {lead}
      <ExampleTabs tabs={tabs} />
    </>
  );
}

/** The Errors list: each entry's leading code link becomes a Badge, the rest stays text. */
function errorList(list: Block): ReactNode {
  return (
    <ul>
      {childrenOf(list).map((item, at) => {
        if (!isValidElement<Block['props']>(item)) {
          return item;
        }
        // 1. An entry that does not open with a link stays as it is.
        const [first, ...rest] = childrenOf(item);
        const link = asLink(first);
        if (!link) {
          return item;
        }

        // 2. `[E_IO](errors.md#e_io) (exit 1): ...` -> a Badge link, then the text.
        return (
          <li key={at}>
            <Badge asChild variant="outline" className="font-mono text-xs no-underline">
              <Link href={link.props.href ?? ''}>{nodeText(link.props.children)}</Link>
            </Badge>
            {rest}
          </li>
        );
      })}
    </ul>
  );
}

function errorSection(body: Block[]): ReactNode {
  const at = body.findIndex((block) => isA(block, Ul));
  if (at === -1) {
    return body;
  }
  return body.map((block, i) => (i === at ? <Fragment key="errors">{errorList(block)}</Fragment> : block));
}

/** One `## ` section of a command or MCP tool page. */
function section(part: Part): ReactNode {
  // 1. The sections the generator gives a shape to; any other stays as it is.
  let body: ReactNode = part.body;
  if (ARGUMENT_SECTIONS.includes(part.title)) {
    body = argumentList(part.body);
  } else if (part.title === EXAMPLES_SECTION) {
    body = exampleTabs(part.body);
  } else if (part.title === ERRORS_SECTION) {
    body = errorSection(part.body);
  }

  // 2. The heading, then the body.
  return (
    <Fragment key={part.id}>
      {part.heading}
      {body}
    </Fragment>
  );
}

/** A command or MCP tool page: the metadata block as a Card, then its sections. */
function referencePage(page: DocsPage, kind: PageKind, blocks: Block[], meta: Meta | null): ReactNode {
  const { lead, parts } = cut(blocks, H2);

  // 1. The metadata block (the first code block above the first section) becomes the Card.
  const tool = meta ? metaText(meta, 'mcp_tool') : '';
  const card = meta && (
    <MetaCard
      key="meta"
      title={page.data.title}
      meta={meta}
      tool={tool === '' ? null : toolPage(tool)}
      commands={kind === PageKind.McpTool ? toolCommands(page.slugs.at(-1)?.replaceAll('-', '_') ?? '') : []}
    />
  );
  const metaAt = meta ? lead.findIndex((block) => isA(block, Pre)) : -1;
  const head = lead.map((block, i) => (i === metaAt ? card : block));

  // 2. Each section, with its parts replaced where it has some.
  return (
    <>
      {head}
      {parts.map(section)}
    </>
  );
}

/** The commands of one list in cli.md: `- [swarmops init](commands/init.md): Create a store ...`. */
function indexCommands(list: Block, parent: string): IndexCommand[] {
  const commands: IndexCommand[] = [];
  for (const item of childrenOf(list)) {
    if (!isValidElement<Block['props']>(item)) {
      continue;
    }
    const [first, ...rest] = childrenOf(item);
    const link = asLink(first);
    if (!link || !isCommandUrl(link.props.href ?? '')) {
      continue;
    }

    // 1. The summary is the text after the link's ": ".
    const url = link.props.href ?? '';
    const name = nodeText(link.props.children);
    const summary = rest.map((node, i) => (i === 0 && typeof node === 'string' ? node.replace(SUMMARY_SEPARATOR, '') : node));

    // 2. The MCP tool, from the command page's metadata.
    const meta = commandMeta(url);
    const toolName = meta ? metaText(meta, 'mcp_tool') : '';
    const tool = toolName === '' ? null : { name: toolName, url: toolPage(toolName)?.url ?? null };
    commands.push({
      name,
      url,
      summary,
      tool,
      text: [name, toolName, parent, nodeText(summary)]
        .join(' ')
        .toLowerCase(),
    });
  }
  return commands;
}

/** One `## Group` of cli.md, or null when its lists hold no command page links. */
function indexGroup(part: Part): IndexGroup | null {
  const { lead, parts } = cut(part.body, H3);
  const families: IndexFamily[] = [];

  // 1. Commands listed directly under the group.
  const direct = lead
    .filter((block) => isA(block, Ul))
    .flatMap((list) => indexCommands(list, ''));
  if (direct.length > 0) {
    families.push({ parent: null, commands: direct });
  }

  // 2. A parent command (`### swarmops blob`), its description, then its subcommands.
  for (const sub of parts) {
    const commands = sub.body
      .filter((block) => isA(block, Ul))
      .flatMap((list) => indexCommands(list, sub.title));
    const summary = sub.body
      .filter((block) => block.type !== Ul)
      .map((block) => block.props.children);
    families.push({ parent: { id: sub.id, name: sub.title, summary }, commands });
  }

  if (!families.some((family) => family.commands.length > 0)) {
    return null;
  }
  return { id: part.id, title: part.title, families };
}

/** cli.md: the sections before the command groups as they are, then the groups as one table. */
function commandIndex(blocks: Block[]): ReactNode {
  const { lead, parts } = cut(blocks, H2);
  const groups: IndexGroup[] = [];
  const out: ReactNode[] = [...lead];
  let tableAt = -1;

  // 1. Group sections go into the table, the others stay as they are.
  for (const part of parts) {
    const group = indexGroup(part);
    if (!group) {
      out.push(section(part));
      continue;
    }
    if (tableAt === -1) {
      tableAt = out.length;
    }
    groups.push(group);
  }
  if (tableAt === -1) {
    return out;
  }

  // 2. The table takes the place of the first group, under a heading of its own.
  out.splice(
    tableAt,
    0,
    <headings.h2 key="commands-heading" id={COMMANDS_ID}>
      Commands
    </headings.h2>,
    <CommandIndex key="commands" groups={groups} />,
  );
  return out;
}

export function ReferenceBody({ page, kind }: { page: DocsPage; kind: PageKind }) {
  const MDX = page.data.body;
  const blocks = blocksOf(MDX({ components: markers }));

  if (kind === PageKind.CommandIndex) {
    return <>{commandIndex(blocks)}</>;
  }
  return <>{referencePage(page, kind, blocks, parseMeta(pageMarkdown(page)))}</>;
}
