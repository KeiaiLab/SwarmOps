// A reference page's metadata block as a Card, in place of the `yaml` fence GitHub shows:
//
//   +-- Command metadata ---------------------------------------+
//   | Group  write        Since  0.1.0      Mutates  yes        |
//   | Identity required  yes                MCP tool  commit -> |
//   | Envelope op  commit Exit codes  0 1 2 4 5                 |
//   | Errors  [E_IDENTITY_REQUIRED] [E_TOO_LARGE] ...           |
//   +-----------------------------------------------------------+
//
// Keys this card does not know (an MCP tool page may carry others) show as they are.
import type { ReactNode } from 'react';
import Link from 'next/link';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import type { Meta, MetaValue } from '@/lib/reference/meta';
import { errorUrl, type PageLink } from '@/lib/reference/pages';

// Shown elsewhere on the page: the title, the description paragraph, the Syntax section.
const HIDDEN_KEYS = ['title', 'description', 'syntax'];

// Known keys, in this order, with their labels; the rest follow in the block's order.
const LABELS: Record<string, string> = {
  group: 'Group',
  since: 'Since',
  mutates: 'Mutates',
  identity_required: 'Identity required',
  mcp_tool: 'MCP tool',
  envelope_op: 'Envelope op',
  exit_codes: 'Exit codes',
  errors: 'Errors',
};

const ERRORS_KEY = 'errors';
const TOOL_KEY = 'mcp_tool';
const NONE = 'none';
const LINK = 'text-primary underline-offset-4 hover:underline';

export interface MetaCardProps {
  title: string;
  meta: Meta;
  // The MCP tool's page, when the command runs a tool that has one.
  tool: PageLink | null;
  // The commands that run this page's tool (MCP tool pages only).
  commands: PageLink[];
}

/** A key the card has no label for: `read_only` -> Read only. */
function label(key: string): string {
  const words = key.replaceAll('_', ' ');
  return LABELS[key] ?? words.charAt(0).toUpperCase() + words.slice(1);
}

/** `["a", 1]` -> code chips; `true` -> yes; `""` -> none. */
function plain(value: MetaValue): ReactNode {
  if (typeof value === 'boolean') {
    return value ? 'yes' : 'no';
  }
  if (Array.isArray(value)) {
    if (value.length === 0) {
      return NONE;
    }
    return (
      <span className="flex flex-wrap gap-1">
        {value.map((item) => (
          <Badge key={String(item)} variant="secondary" className="font-mono">
            {item}
          </Badge>
        ))}
      </span>
    );
  }
  if (value === '') {
    return NONE;
  }
  return <code>{value}</code>;
}

/** Error codes as Badges, each a link to its entry on the errors page. */
export function ErrorBadge({ code }: { code: string }) {
  return (
    <Badge asChild variant="outline" className="font-mono">
      <Link href={errorUrl(code)}>{code}</Link>
    </Badge>
  );
}

/** One value, linked where the page can link it. */
function rendered(key: string, value: MetaValue, tool: PageLink | null): ReactNode {
  // 1. Errors link to their entries.
  if (key === ERRORS_KEY && Array.isArray(value) && value.length > 0) {
    return (
      <span className="flex flex-wrap gap-1">
        {value.map((code) => (
          <ErrorBadge key={String(code)} code={String(code)} />
        ))}
      </span>
    );
  }

  // 2. The MCP tool links to its page, when it has one.
  if (key === TOOL_KEY && typeof value === 'string' && value !== '' && tool) {
    return (
      <Link href={tool.url} className={LINK}>
        <code>{value}</code>
      </Link>
    );
  }
  return plain(value);
}

export function MetaCard({ title, meta, tool, commands }: MetaCardProps) {
  // 1. Known keys first, in LABELS order, then the rest as the block lists them.
  const known = Object.keys(LABELS).filter((key) => key in meta);
  const rest = Object.keys(meta).filter((key) => !(key in LABELS) && !HIDDEN_KEYS.includes(key));
  const keys = [...known, ...rest];

  // 2. One term per key; a tool page adds the commands that run it.
  return (
    <Card className="not-prose my-6 gap-4 py-4" role="group" aria-label={`${title} metadata`}>
      <CardHeader className="px-4">
        <CardTitle className="text-sm">Metadata</CardTitle>
      </CardHeader>
      <CardContent className="px-4">
        <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 text-sm">
          {keys.map((key) => (
            <div key={key} className="contents">
              <dt className="text-muted-foreground">{label(key)}</dt>
              <dd>{rendered(key, meta[key], tool)}</dd>
            </div>
          ))}
          {commands.length > 0 && (
            <div className="contents">
              <dt className="text-muted-foreground">CLI command</dt>
              <dd className="flex flex-wrap gap-2">
                {commands.map((command) => (
                  <Link key={command.url} href={command.url} className={LINK}>
                    <code>{command.title}</code>
                  </Link>
                ))}
              </dd>
            </div>
          )}
        </dl>
      </CardContent>
    </Card>
  );
}
