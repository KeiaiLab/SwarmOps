// The product header's theme control: three buttons (System, Light, Dark), one pressed. Server
// rendered; the site script (lib/site-script.ts) marks the stored choice and handles the clicks.
// Under 1024 px each button shows its icon, its name stays readable by assistive technology.
import { Monitor, Moon, Sun } from 'lucide-react';

const CHOICES = [
  { theme: 'system', name: 'System', Icon: Monitor },
  { theme: 'light', name: 'Light', Icon: Sun },
  { theme: 'dark', name: 'Dark', Icon: Moon },
] as const;

// The default before the script runs: the system's preference.
const DEFAULT_THEME = 'system';

export function ThemeToggle() {
  return (
    <div role="group" aria-label="Theme" className="flex items-center rounded-md border border-input">
      {CHOICES.map(({ theme, name, Icon }) => (
        <button
          key={theme}
          type="button"
          data-theme-set={theme}
          aria-pressed={theme === DEFAULT_THEME}
          title={name}
          className="inline-flex min-h-11 min-w-7 items-center justify-center gap-1 px-1 sm:min-w-8 lg:px-2 text-sm text-muted-foreground transition-colors duration-120 hover:text-foreground aria-pressed:bg-muted aria-pressed:text-foreground [&_svg]:size-4"
        >
          <Icon aria-hidden="true" />
          <span className="max-lg:sr-only">{name}</span>
        </button>
      ))}
    </div>
  );
}
