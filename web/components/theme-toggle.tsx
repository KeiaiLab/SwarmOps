'use client';
// Switches between dark and light. Both icons render, and CSS shows the one for the current
// theme, so the server's HTML matches the client's before the theme is known. A plain button,
// not the shadcn one, keeps class merging out of the product pages' script.
import { Moon, Sun } from 'lucide-react';
import { useTheme } from 'next-themes';

export function ThemeToggle() {
  const { resolvedTheme, setTheme } = useTheme();

  return (
    <button
      type="button"
      aria-label="Toggle dark mode"
      className="inline-flex size-9 items-center justify-center rounded-md text-fd-muted-foreground hover:bg-fd-accent hover:text-fd-accent-foreground [&_svg]:size-4"
      onClick={() => setTheme(resolvedTheme === 'dark' ? 'light' : 'dark')}
    >
      <Sun className="dark:hidden" />
      <Moon className="hidden dark:block" />
    </button>
  );
}
