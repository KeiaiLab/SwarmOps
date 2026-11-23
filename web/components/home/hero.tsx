// The first screen: status line, the one h1, the benefit sentence, the install line with its
// facts, and the action row; the page's opening, not one of the sections that follow it. At
// 390 px the order is h1, sentence, install line, primary action; the facts follow.
//
//   Pre-release · v0.13.0 · 2026-11-23 · Linux x86_64 and aarch64 · macOS aarch64 · MIT
//   # Version control for swarms of coding agents
//   Agents commit into one store at once, a queue lands only what passed ...
//   [ curl ... | bash        (copy) ]   installs swarmops and so
//   [Quick start ->] What it is  When   into ~/.local/bin ...
import { CopyLine } from '@/components/copy-line';
import { LATEST_RELEASE, releaseUrl } from '@/lib/release';
import { INSTALL_LINE, PLATFORMS, SITE_TITLE } from '@/lib/site';
import { C, LINK, LINK_44, PrimaryAction } from './parts';

// The first release with a macOS archive: 0.11.0 and older hold Linux archives only
// (`MACOS_AFTER` in scripts/install.sh).
const MACOS_SINCE = '0.12.0';

// What the install facts say of each platform: how far it is tested, or what it is and since
// when (README "Install", goal G8).
const NOTES: Record<string, string> = {
  'Linux x86_64': 'tested',
  'Linux aarch64': 'smoke-tested under emulation',
  'macOS aarch64': 'Apple silicon',
};

// The release a system's first archive is in; a system not named here is in every release.
const SINCE: Record<string, string> = { macOS: MACOS_SINCE };

const LINUX = 'Linux';
const MACOS = 'macOS';
const GLIBC = 'glibc 2.34 or newer';
const INSTALL_GUIDE_URL = '/docs/guides/install';
const INSTALLER_URL = '/install.sh';
// Each release since v0.8.0 also carries a .deb and an .rpm for each architecture.
const PACKAGES_URL = '/docs/guides/install#how-do-i-install-the-deb-or-rpm-package';
const GOAL_G8_URL = '/docs/goals#goals';
const ROADMAP_URL = '/docs/roadmap';
// The ledger section holding the measured figure beside the benefit sentence.
const MEASURED_URL = '/docs/perf#d2-8-gated-runs--2026-11-09';

/** The platforms of one system, as `PLATFORMS` names them: `Linux x86_64`, `Linux aarch64`. */
function platformsOf(os: string): string[] {
  const found = PLATFORMS.filter((platform) => platform.startsWith(`${os} `));
  if (found.length === 0) {
    throw new Error(`hero: no platform of ${os}`);
  }
  return found;
}

/** One system's platforms in a phrase, the system named once: `Linux x86_64 and aarch64`. */
function phrase(os: string, text: (platform: string) => string): string {
  const parts = platformsOf(os).map((platform, i) => {
    const said = text(platform);
    return i === 0 ? said : said.replace(`${os} `, '');
  });
  return parts.join(' and ');
}

/** `Linux x86_64 (tested) and aarch64 (smoke-tested under emulation)`, from `PLATFORMS`. */
function platformText(os: string): string {
  return phrase(os, (platform) => {
    const note = NOTES[platform];
    if (!note) {
      throw new Error(`hero: no note for the platform "${platform}"`);
    }
    return `${platform} (${note})`;
  });
}

/** Whether release `version` is `since` or newer; both are `x.y.z`. */
function atLeast(version: string, since: string): boolean {
  const [ours, theirs] = [version, since].map((v) => v.split('.').map(Number));
  const at = ours.findIndex((n, i) => n !== theirs[i]);
  return at < 0 || ours[at] > theirs[at];
}

/** The systems release `version` holds an archive for, in `PLATFORMS`' order: `Linux`, `macOS`. */
function systemsOf(version: string): string[] {
  const systems = [...new Set(PLATFORMS.map((platform) => platform.split(' ')[0]))];
  return systems.filter((os) => !SINCE[os] || atLeast(version, SINCE[os]));
}

function StatusLine() {
  const release = LATEST_RELEASE;
  return (
    <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">
      <span aria-hidden="true" className="size-2 flex-none rounded-full bg-ring" />
      {release.preRelease && <span className="font-semibold text-foreground">Pre-release</span>}
      <a className={LINK + ' figure'} href={releaseUrl(release)}>
        v{release.version}
      </a>
      <span className="figure">{release.date}</span>
      {systemsOf(release.version).map((os) => (
        <span key={os}>{phrase(os, (platform) => platform)}</span>
      ))}
      <span>MIT</span>
    </p>
  );
}

function InstallFacts() {
  return (
    <div className="text-sm text-muted-foreground">
      {/* 1. What the line does, one fact a row. */}
      <ul className="list-none border-t-2 border-rule-strong p-0">
        <li className="border-b border-border py-2">
          Installs <C>swarmops</C> and <C>so</C>, its short name
        </li>
        <li className="border-b border-border py-2">
          Into <C>~/.local/bin</C>
        </li>
        <li className="border-b border-border py-2">
          Checked against the release&apos;s <C>SHA256SUMS</C>
        </li>
        <li className="border-b border-border py-2">
          {platformText(LINUX)}, {GLIBC}; {platformText(MACOS)}; no Windows build (
          <a className={LINK} href={GOAL_G8_URL}>
            goal G8
          </a>
          ,{' '}
          <a className={LINK} href={ROADMAP_URL}>
            roadmap
          </a>
          )
        </li>
        <li className="border-b border-border py-2">Run it again to update</li>
      </ul>

      {/* 2. The other ways in, the packages first, and the script itself. */}
      <p className="mt-2 flex flex-wrap gap-x-6">
        <a className={LINK_44} href={PACKAGES_URL}>
          .deb and .rpm packages
        </a>
        <a className={LINK_44} href={INSTALL_GUIDE_URL}>
          Other install methods
        </a>
        <a className={LINK_44} href={INSTALLER_URL}>
          The installer as text
        </a>
      </p>
    </div>
  );
}

export function Hero() {
  return (
    <div id="top" className="page pt-16 pb-20 max-sm:pt-8 max-sm:pb-14">
      {/* 1. Status, title and the sentence that says what it does. */}
      <StatusLine />
      <h1
        id="top-title"
        className="mt-6 max-w-[16ch] font-serif text-3xl font-normal tracking-tight text-foreground sm:text-4xl"
      >
        {SITE_TITLE}
      </h1>
      <p className="mt-4 max-w-(--measure) text-lg text-(--text)">
        Agents commit into one store at once, a queue lands only what passed its checks, and work reaches GitHub or
        your own server. 50 agents: 25.0 lands/s, none lost (
        <a className={LINK} href={MEASURED_URL}>
          perf
        </a>
        ).
      </p>

      {/* 2. The install line and the action row; the facts beside them on wide screens. */}
      <div className="mt-10 grid grid-cols-1 gap-x-16 gap-y-8 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
        <div className="min-w-0">
          <CopyLine text={INSTALL_LINE} label="the install command" />
          <div className="mt-6 flex flex-wrap items-center gap-x-6 gap-y-2">
            <PrimaryAction className="max-sm:w-full" />
            <a className={LINK_44} href="/product">
              What it is
            </a>
            <a className={LINK_44} href="#fit">
              When not to use it
            </a>
          </div>
        </div>
        <InstallFacts />
      </div>
    </div>
  );
}
