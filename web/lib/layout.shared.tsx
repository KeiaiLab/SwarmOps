// The docs header: the logo, Product · Docs · Roadmap · Changelog, and GitHub.
import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { GITHUB_URL, NAV_LINKS, SITE_NAME } from './site';

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
    links: NAV_LINKS.map((link) => ({ ...link, active: 'nested-url' as const })),
    githubUrl: GITHUB_URL,
  };
}
