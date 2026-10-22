'use client';
// The docs' theme (dark/light, following the system first) and static search dialog.
import dynamic from 'next/dynamic';
import { RootProvider } from 'fumadocs-ui/provider/next';
import { type ReactNode } from 'react';

// The dialog and its search engine load when the reader first opens search.
const SearchDialog = dynamic(() => import('@/components/search'), { ssr: false });

export function Provider({ children }: { children: ReactNode }) {
  return <RootProvider search={{ SearchDialog }}>{children}</RootProvider>;
}
