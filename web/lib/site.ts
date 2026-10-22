// Site-wide names and addresses.
import { REPO_URL } from './docs-paths';

export const SITE_NAME = 'SwarmOps';
export const SITE_URL = 'https://swarmops.cc';
// README.md, the tagline.
export const SITE_TAGLINE =
  'A git replacement for coding-agent swarms: many writers, one store, one JSON envelope per command.';
export const GITHUB_URL = REPO_URL;
export const CHANGELOG_URL = '/docs/changelog';

/** The header's sections, on every page. */
export const NAV_LINKS = [
  { text: 'Product', url: '/product' },
  { text: 'Docs', url: '/docs' },
  { text: 'Roadmap', url: '/roadmap' },
  { text: 'Changelog', url: CHANGELOG_URL },
];
