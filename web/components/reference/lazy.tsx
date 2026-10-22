// The reference pages' client components, each in a chunk of its own: a docs page loads one only
// when it renders it, so the other docs pages carry none of their script.
'use client';

import dynamic from 'next/dynamic';

export const ArgumentAccordion = dynamic(() => import('./argument-accordion').then((m) => m.ArgumentAccordion));
export const ExampleTabs = dynamic(() => import('./example-tabs').then((m) => m.ExampleTabs));
export const CommandIndex = dynamic(() => import('./command-index').then((m) => m.CommandIndex));
