// Site-wide names, addresses and link lists: the one source both frames (product and docs) read.
import { REPO_URL } from './docs-paths';

export const SITE_NAME = 'SwarmOps';
export const SITE_URL = 'https://swarmops.cc';

// The claim, once (site design, section 0).
export const SITE_TITLE = 'Version control for swarms of coding agents';
export const SITE_TAGLINE =
  'Version control for swarms of coding agents: many writers, one store, one JSON envelope per command.';

// Install: the installer line and the platforms it serves (scripts/install.sh).
export const INSTALL_LINE = 'curl -fsSL https://get.swarmops.cc/install.sh | bash';
export const PLATFORMS = ['Linux x86_64', 'Linux aarch64'];

// Addresses other pages link to.
export const QUICK_START_URL = '/docs/guides/quick-start';
export const CHANGELOG_URL = '/docs/changelog';
export const GITHUB_URL = REPO_URL;
export const ASK_URL = `${GITHUB_URL}/issues/new/choose`;

/** A link of the header or footer; `external` leaves the site. */
interface SiteLink {
  text: string;
  url: string;
  external?: boolean;
}

/** The header's sections, on every page, each once. */
export const NAV_LINKS: SiteLink[] = [
  { text: 'Docs', url: '/docs' },
  { text: 'Product', url: '/product' },
  { text: 'Roadmap', url: '/roadmap' },
  { text: 'Changelog', url: CHANGELOG_URL },
  { text: 'GitHub', url: GITHUB_URL, external: true },
];

// A file on the default branch of the repository host.
const repoFile = (name: string) => `${GITHUB_URL}/blob/stable/${name}`;

/** The footer's four columns: 16 destinations (site design, section 2.5). */
export const FOOTER_COLUMNS: { title: string; links: SiteLink[] }[] = [
  {
    title: 'Start',
    links: [
      { text: 'Quick start', url: QUICK_START_URL },
      { text: 'All guides', url: '/docs' },
      { text: 'Reference', url: '/docs/reference/cli' },
    ],
  },
  {
    title: 'Project',
    links: [
      { text: 'Roadmap', url: '/docs/roadmap' },
      { text: 'Changelog', url: CHANGELOG_URL },
      { text: 'Goals', url: '/docs/goals' },
      { text: 'Measurements', url: '/docs/perf' },
      { text: 'Known limitations', url: '/docs/known-limitations' },
    ],
  },
  {
    title: 'Community',
    links: [
      { text: 'Ask or report', url: ASK_URL, external: true },
      { text: 'Contributing', url: repoFile('CONTRIBUTING.md'), external: true },
      { text: 'Code of conduct', url: repoFile('CODE_OF_CONDUCT.md'), external: true },
    ],
  },
  {
    title: 'Policies',
    links: [
      { text: 'MIT licence', url: repoFile('LICENSE'), external: true },
      { text: 'Security', url: repoFile('SECURITY.md'), external: true },
      { text: 'Versioning', url: '/docs/policies/versioning-and-stability' },
      { text: 'Clean room', url: '/docs/policies/clean-room' },
      { text: 'Source on GitHub', url: GITHUB_URL, external: true },
    ],
  },
];

