// The docs frame's header and sidebar links: the same mark, NAV_LINKS and release link as the
// product header (components/site-header.tsx), and a three-state theme switch.
import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { LATEST_RELEASE, releaseUrl } from './release';
import { NAV_LINKS, SITE_NAME } from './site';

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <img src="/logo.svg" alt="" width={24} height={24} />
          <span className="font-semibold">{SITE_NAME}</span>
        </>
      ),
    },

    // The five sections, then the release, which leads to its changelog entry.
    links: [
      ...NAV_LINKS.map((link) => ({ ...link, active: 'nested-url' as const })),
      {
        text: <span className="figure">v{LATEST_RELEASE.version}</span>,
        url: releaseUrl(LATEST_RELEASE),
        active: 'none' as const,
      },
    ],
    themeSwitch: { mode: 'light-dark-system' },
  };
}
