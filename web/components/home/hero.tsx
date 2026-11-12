// The first screen: status line, the one h1, the benefit sentence, the install line with its
// facts, and the action row. At 390 px the order is h1, sentence, install line, primary action;
// the facts follow.
//
//   Pre-release · v0.11.0 · 2026-11-09 · Linux ... · MIT
//   # Version control for swarms of coding agents
//   Designed for large pools of coding agents ...
//   [ curl ... | bash        (copy) ]   installs swarmops and so
//   [Quick start ->] What it is  When   into ~/.local/bin ...
import { CopyLine } from '@/components/copy-line';
import { LATEST_RELEASE, releaseUrl } from '@/lib/release';
import { INSTALL_LINE, PLATFORMS, SITE_TITLE } from '@/lib/site';
import { C, LINK, LINK_44, PrimaryAction } from './parts';

// How far each platform of the release is tested (README "Install", goal G8).
const TESTED: Record<string, string> = {
  'Linux x86_64': 'tested',
  'Linux aarch64': 'smoke-tested under emulation',
};

const OS_PREFIX = 'Linux ';
const GLIBC = 'glibc 2.34 or newer';
const INSTALL_GUIDE_URL = '/docs/guides/install';
const INSTALLER_URL = '/install.sh';
const GOAL_G8_URL = '/docs/goals#goals';
const ROADMAP_URL = '/docs/roadmap';
// The ledger section holding the measured figure beside the benefit sentence.
const MEASURED_URL = '/docs/perf#d2-8-gated-runs--2026-11-09';

/** `Linux x86_64 (tested) and aarch64 (smoke-tested under emulation)`, from `PLATFORMS`. */
function platformText(): string {
  const parts = PLATFORMS.map((platform, i) => {
    const tested = TESTED[platform];
    if (!tested) {
      throw new Error(`hero: no test note for the platform "${platform}"`);
    }
    const name = i === 0 ? platform : platform.replace(OS_PREFIX, '');
    return `${name} (${tested})`;
  });
  return parts.join(' and ');
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
      <span>{platformText()}</span>
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
          {platformText()}, {GLIBC}; no macOS or Windows build yet (
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

      {/* 2. The other ways in, and the script itself. */}
      <p className="mt-2 flex flex-wrap gap-x-6">
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
    <section id="top" aria-labelledby="top-title" className="page pt-16 pb-20 max-sm:pt-8 max-sm:pb-14">
      {/* 1. Status, title and the sentence that says what it does. */}
      <StatusLine />
      <h1
        id="top-title"
        className="mt-6 max-w-[16ch] font-serif text-3xl font-normal tracking-tight text-foreground sm:text-4xl"
      >
        {SITE_TITLE}
      </h1>
      <p className="mt-4 max-w-(--measure) text-lg text-(--text)">
        Designed for large pools of coding agents: their processes commit into one store at once; every command
        answers in one JSON envelope, and the work goes back to git. Measured today at 50 concurrent writing
        processes on one host: 506.8 commits/s, nothing lost; 50 agents landing: 25.0 lands/s, nothing lost (
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
    </section>
  );
}
