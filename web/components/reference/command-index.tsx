// The command index as one table, a row group per command group, with a filter above it:
//
//   [ Filter commands: land            ]
//   Command               MCP tool   Summary
//   Write -----------------------------------------------
//   swarmops land         land       Merge a change three-way onto a ref ...
//
// Every row is in the HTML; the filter only hides rows, so the page reads the same without script.
'use client';

import { useId, useState, type ReactNode } from 'react';
import Link from 'next/link';

export interface IndexCommand {
  name: string;
  url: string;
  summary: ReactNode;
  tool: { name: string; url: string | null } | null;
  // What the filter matches: name, tool, summary and the parent command, lower case.
  text: string;
}

/** A parent command (`swarmops blob`) and its subcommands, or a group's commands without one. */
export interface IndexFamily {
  parent: { id: string; name: string; summary: ReactNode } | null;
  commands: IndexCommand[];
}

export interface IndexGroup {
  id: string;
  title: string;
  families: IndexFamily[];
}

const COLUMNS = 3;
const LINK = 'text-primary underline-offset-4 hover:underline';

function ToolCell({ tool }: { tool: IndexCommand['tool'] }) {
  if (!tool) {
    return null;
  }
  const name = <code>{tool.name}</code>;
  return tool.url ? <Link href={tool.url} className={LINK}>{name}</Link> : name;
}

export function CommandIndex({ groups }: { groups: IndexGroup[] }) {
  const inputId = useId();
  const [query, setQuery] = useState('');
  const needle = query
    .trim()
    .toLowerCase();
  const shows = (command: IndexCommand) => needle === '' || command.text.includes(needle);
  const all = groups.flatMap((group) => group.families.flatMap((family) => family.commands));
  const matches = all.filter(shows).length;

  return (
    <div className="not-prose my-6">
      {/* 1. The filter, with a live count for screen readers. */}
      <div className="mb-3 flex flex-wrap items-center gap-3">
        <label htmlFor={inputId} className="text-sm font-medium">
          Filter commands
        </label>
        <input
          id={inputId}
          type="search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="name, tool or words"
          className="h-9 w-64 max-w-full rounded-md border border-input bg-background px-3 text-sm"
        />
        <span className="text-sm text-muted-foreground" aria-live="polite">
          {matches} of {all.length} commands
        </span>
      </div>

      {/* 2. One row group per command group; a group with no match is hidden. */}
      <div className="relative overflow-auto">
        <table className="w-full border-collapse text-sm">
          <thead>
            <tr className="border-b text-left">
              <th scope="col" className="py-2 pe-4 whitespace-nowrap">Command</th>
              <th scope="col" className="py-2 pe-4 whitespace-nowrap">MCP tool</th>
              <th scope="col" className="py-2">Summary</th>
            </tr>
          </thead>
          {groups.map((group) => {
            const hidden = !group.families.some((family) => family.commands.some(shows));
            return (
              <tbody key={group.id} hidden={hidden}>
                <tr>
                  <th id={group.id} scope="rowgroup" colSpan={COLUMNS} className="scroll-m-28 pt-5 pb-2 text-left text-base font-semibold">
                    {group.title}
                  </th>
                </tr>
                {group.families.flatMap((family) => [
                  family.parent && (
                    <tr key={family.parent.id} hidden={!family.commands.some(shows)} className="border-t">
                      <td id={family.parent.id} className="scroll-m-28 py-2 pe-4 align-top font-medium">
                        <code>{family.parent.name}</code>
                      </td>
                      <td className="py-2 pe-4" />
                      <td className="py-2 text-muted-foreground">{family.parent.summary}</td>
                    </tr>
                  ),
                  ...family.commands.map((command) => (
                    <tr key={command.url} hidden={!shows(command)} className="border-t">
                      <td className={`py-2 pe-4 align-top whitespace-nowrap ${family.parent ? 'ps-4' : ''}`}>
                        <Link href={command.url} className={LINK}>
                          <code>{command.name}</code>
                        </Link>
                      </td>
                      <td className="py-2 pe-4 align-top">
                        <ToolCell tool={command.tool} />
                      </td>
                      <td className="py-2 align-top">{command.summary}</td>
                    </tr>
                  )),
                ])}
              </tbody>
            );
          })}
        </table>
      </div>
      {matches === 0 && <p className="mt-3 text-sm text-muted-foreground">No command matches.</p>}
    </div>
  );
}
