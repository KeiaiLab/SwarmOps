// After a docs page's body: edit its file on GitHub, or report a problem with it as an issue
// whose title names the file (the bug form, `docs: docs/guides/install.md`).
import { EditOnGitHub } from 'fumadocs-ui/layouts/docs/page';
import { githubUrl } from '@/lib/docs-paths';
import { GITHUB_URL } from '@/lib/site';

// The repository's bug report form, and the prefix of a docs issue's title.
const BUG_FORM = 'bug_report.yml';
const TITLE_PREFIX = 'docs: ';

/** A new issue on the bug form, its title naming the file. */
export function reportUrl(repoPath: string): string {
  return `${GITHUB_URL}/issues/new?template=${BUG_FORM}&title=${encodeURIComponent(TITLE_PREFIX + repoPath)}`;
}

export function PageActions({ source }: { source: string }) {
  return (
    <div className="not-prose mt-12 flex flex-wrap items-center gap-x-4 gap-y-2 border-t pt-4 text-sm">
      <EditOnGitHub href={githubUrl(source)} />
      <a
        href={reportUrl(source)}
        className="text-fd-muted-foreground underline-offset-4 transition-colors duration-120 hover:text-fd-primary hover:underline"
      >
        Report a problem with this page
      </a>
    </div>
  );
}
