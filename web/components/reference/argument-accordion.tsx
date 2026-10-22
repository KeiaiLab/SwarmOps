// A page's `### <argument>` subsections as Accordion entries, all open at first, so the page reads
// in full without script and every argument's anchor is in the HTML.
'use client';

import type { ReactNode } from 'react';
import { Accordion, AccordionContent, AccordionItem, AccordionTrigger } from './ui/accordion';

export interface ArgumentEntry {
  // The subsection heading's id: the entry's value and its anchor.
  id: string;
  label: ReactNode;
  content: ReactNode;
}

export function ArgumentAccordion({ entries }: { entries: ArgumentEntry[] }) {
  return (
    <Accordion type="multiple" defaultValue={entries.map((entry) => entry.id)} className="my-4">
      {entries.map((entry) => (
        <AccordionItem key={entry.id} value={entry.id}>
          <AccordionTrigger id={entry.id} className="text-base">
            {entry.label}
          </AccordionTrigger>
          <AccordionContent className="text-base [&>:first-child]:mt-0">{entry.content}</AccordionContent>
        </AccordionItem>
      ))}
    </Accordion>
  );
}
