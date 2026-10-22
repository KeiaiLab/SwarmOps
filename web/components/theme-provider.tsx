'use client';
// Dark/light on the product pages, as the docs' provider does it: a `dark` class on <html>, the
// system's preference until the reader picks one.
import { ThemeProvider as NextThemes } from 'next-themes';
import type { ReactNode } from 'react';

export function ThemeProvider({ children }: { children: ReactNode }) {
  return (
    <NextThemes attribute="class" defaultTheme="system" enableSystem disableTransitionOnChange>
      {children}
    </NextThemes>
  );
}
