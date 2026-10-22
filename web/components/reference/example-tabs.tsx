// A page's `### CLI` and `### MCP` example subsections as Tabs. Every panel is in the HTML (hidden
// when inactive, not left out), so search and the page's anchors see both; a link to `#mcp`
// selects the MCP tab and scrolls to it.
'use client';

import { useEffect, useState, type ReactNode } from 'react';
import { Tabs, TabsContent, TabsList, TabsTrigger } from './ui/tabs';

export interface ExampleTab {
  // The subsection heading's id: the tab's value and its anchor.
  id: string;
  label: ReactNode;
  content: ReactNode;
}

export function ExampleTabs({ tabs }: { tabs: ExampleTab[] }) {
  const [value, setValue] = useState(tabs[0]?.id ?? '');

  // A URL hash naming a tab selects it, on load and on every in-page link.
  useEffect(() => {
    const select = () => {
      const id = decodeURIComponent(window.location.hash.slice(1));
      if (!tabs.some((tab) => tab.id === id)) {
        return;
      }
      setValue(id);
      requestAnimationFrame(() => document.getElementById(id)?.scrollIntoView());
    };
    select();
    window.addEventListener('hashchange', select);
    return () => window.removeEventListener('hashchange', select);
  }, [tabs]);

  return (
    <Tabs value={value} onValueChange={setValue} className="my-6">
      <TabsList>
        {tabs.map((tab) => (
          <TabsTrigger key={tab.id} value={tab.id} className="px-4">
            {tab.label}
          </TabsTrigger>
        ))}
      </TabsList>
      {tabs.map((tab) => (
        <TabsContent key={tab.id} value={tab.id} forceMount className="data-[state=inactive]:hidden">
          <div id={tab.id} className="scroll-m-28">
            {tab.content}
          </div>
        </TabsContent>
      ))}
    </Tabs>
  );
}
