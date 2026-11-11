// The footer of both frames (product and docs): four columns of text links, then where to ask,
// where to report a vulnerability, the licence and the docs index for programs.
import Link from 'next/link';
import { ASK_URL, FOOTER_COLUMNS, GITHUB_URL, SITE_NAME } from '@/lib/site';

// Security policy on the repository host: how to report a vulnerability privately.
const SECURITY_URL = `${GITHUB_URL}/blob/stable/SECURITY.md`;
const LLMS_URL = '/llms.txt';

const LINK = 'inline-flex min-h-6 items-center text-text underline-offset-4 transition-colors duration-120 hover:text-primary hover:underline';
const QUIET = 'text-primary underline underline-offset-4';

export function SiteFooter() {
  return (
    <footer className="mt-auto border-t border-rule-strong bg-card">
      <div className="page flex flex-col gap-10 py-12">
        {/* 1. The site map: four columns, two under 640 px. */}
        <div className="grid grid-cols-2 gap-x-6 gap-y-8 md:grid-cols-4">
          {FOOTER_COLUMNS.map((column) => (
            <section key={column.title} aria-labelledby={`footer-${column.title}`}>
              <h2 id={`footer-${column.title}`} className="caption text-muted-foreground">
                {column.title}
              </h2>
              <ul className="mt-3 flex flex-col gap-1 text-sm">
                {column.links.map((link) => (
                  <li key={link.url}>
                    {link.external ? (
                      <a className={LINK} href={link.url}>
                        {link.text}
                      </a>
                    ) : (
                      <Link className={LINK} href={link.url}>
                        {link.text}
                      </Link>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </div>

        {/* 2. Where to ask and where to report, as text a reader without scripts gets. */}
        <p className="max-w-(--measure) text-sm text-muted-foreground">
          Questions and problems:{' '}
          <a className={QUIET} href={ASK_URL}>
            open an issue
          </a>
          . A vulnerability: report it privately, as{' '}
          <a className={QUIET} href={SECURITY_URL}>
            SECURITY.md
          </a>{' '}
          describes.
        </p>

        {/* 3. Licence and the docs index for programs, on the ledger's last rule. */}
        <div className="flex flex-wrap items-center justify-between gap-x-6 gap-y-2 border-t pt-6 text-sm text-muted-foreground">
          <span className="caption">{SITE_NAME}, MIT licence</span>
          <a className={QUIET} href={LLMS_URL}>
            Docs index for programs
          </a>
        </div>
      </div>
    </footer>
  );
}
