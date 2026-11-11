# swarmops.cc, the website

A static site, built with Next.js 16 (`output: "export"`), Fumadocs for `/docs`, Tailwind 4 and
shadcn/ui components. Every page is HTML in `out/`; there is no server runtime. The site
workflow deploys `out/` to swarmops.cc (see Deploy).

```sh
just web-install   # npm ci: the only step that needs the network
just web-build     # next build, then scripts/postbuild.mjs; writes out/
just web-check     # npm run check: the selftests, then check-site.mjs over out/ and check-tokens.mjs
```

`npm run dev` serves the site with live reload on port 3000.

## Where pages come from

| Route | Source |
| --- | --- |
| `/` | `app/(product)/page.tsx` |
| `/product/**` | `content/product/**/*.md`, front matter `title` and `description` |
| `/roadmap` | `content/roadmap.md`, same front matter |
| `/docs/**` | `../docs/**/*.md` and `../CHANGELOG.md`, as they are in the repository |
| `/api/search` | the docs search index, built from the same pages |
| `/api/lookup` | the exact-name table: every command page, every MCP tool page and every `E_*` code of the errors page, with its address |
| `/llms.txt` | the docs index for programs: one line per page, linking its Markdown twin |

Product and docs pages share one frame: the header (its links are `NAV_LINKS` in `lib/site.ts`:
Docs, Product, Roadmap, Changelog, GitHub; then search, the version linked to its changelog entry
and the theme control) and the footer (four columns, 16 destinations, also in `lib/site.ts`).
Product pages load no theme library: one inline script, hashed into the page's policy like any
other, sets the theme before paint (`localStorage` key `theme`: system, light or dark) and wires
the theme control, the copy buttons and the search shortcut. Home's goals table is read from
`../docs/goals.md` at build time (`lib/goals.ts`); the build fails when the table, a named goal
or a cell is missing.

Reference pages (docs/reference/commands/, docs/reference/mcp-tools/, docs/reference/cli.md) render through components/reference/: metadata Card, argument Accordion, CLI/MCP example Tabs, error Badges, and a filtered command table.

A docs page keeps its repository path: `docs/guides/install.md` is `/docs/guides/install`,
a `README.md` stands for its folder, `CHANGELOG.md` is `/docs/changelog`. Its title is its first
`# ` heading, its description its first paragraph. `docs/superpowers/`, `docs/reference/notes/`
and `docs/examples/` are never pages. `lib/docs-paths.ts` holds these rules.

Relative links are rewritten at build time (`lib/remark-repo-links.ts`): a link to another docs
page goes to its URL, `docs/assets/` to `/assets/docs/`, and anything else in the repository to
its GitHub page. Headings keep GitHub's ids (github-slugger), so `page.md#section` works on both.

The sidebar has one tab per group of `docs-nav.json`: a page joins the first group that names it
in `files` or holds it under one of its `dirs`, else the `fallback` group, so a new docs file
appears without an edit here.

## Security and budget

`next build` copies `public/` into `out/`: the icons, `site.webmanifest`, `og-image.png`,
`/explainers/`, `THIRD-PARTY-NOTICES.txt`, `.well-known/security.txt`, `_headers`, `_redirects`
and `install.sh`. `scripts/postbuild.mjs` copies `../scripts/install.sh` over `out/install.sh`
(get.swarmops.cc serves it) and `../docs/assets/` to `out/assets/docs/`. It then writes a
`<meta http-equiv="Content-Security-Policy">` into every page, which allows that page's inline
scripts by sha256 and no other inline script. `public/_headers` keeps the site-wide headers
(HSTS, `frame-ancestors 'none'`, the noindex rules for pages.dev and get.swarmops.cc); its
looser `script-src` lets the per-page policy bind.

`scripts/markdown-twins.mjs` then copies each docs page's source file beside its page (`/docs/guides/install.md`, served as text/markdown by `_headers`) and points llms.txt at those copies; a docs page without exactly one "View as Markdown" link fails the build.

## Checks

`npm run check` (what `just web-check` runs) runs, in this order:

1. `scripts/check-site.mjs --selftest`: builds a small site in a temporary directory that passes
   every rule below, then plants one violation per rule; it fails unless each rule reports its
   violation and none reports the clean site.
2. `scripts/check-site.mjs` over `out/`. It fails on a missing docs page, a broken link or anchor,
   a link inside a link, a script, stylesheet or font from another origin, an inline script the
   page's policy does not allow, product pages over the first-load JS budget, or a missing served
   file. Then the rules of the site design, each a function named in the script's header:
   `structure`, `header-links`, `footer-map`, `release-links`, `home-first-screen`,
   `home-primary`, `home-order`, `feature-words`, `home-words`, `copy-control`, `platforms`,
   `mcp-command`, `next-version`, `tool-comparison`, `comparative-link`, `praise-words`,
   `blockquote-source`, `image-text`, `names`, `motion`, `literal-size`, `literal-space`, `llms`,
   `twin-links`, `lookup`, `addresses`, `readme-home-drift` and `home-transfer` (it prints Home's
   gzip transfer; above 300 kB fails). A rule listed in `ENFORCED` fails the run; any other
   prints its findings as `PENDING` (at most 20 each; `--pending` prints all) until a fix clears
   them and the rule joins the list. No input (no page, no table, no file) is a failure.
3. `scripts/check-tokens.mjs --selftest`, then `scripts/check-tokens.mjs`: the contrast ratio of
   every colour pair the design declares, in both themes of `app/global.css`: text tokens on
   background, card and muted at least 4.5, control edges (input, ring, rule-strong) at least 3,
   and the label on a primary button at least 4.5. A missing token fails.

`addresses.txt` lists every page address the site has published. A line that is neither a page
in `out/` nor the source of a rule in `_redirects` fails `addresses`: a page that moves keeps its
old address as a redirect. After adding pages, `node scripts/check-site.mjs --write-addresses`
rewrites the list from `out/`; review the diff, as a removed line is a published address gone.

The budget is a ceiling taken from measurement, not a goal: product pages (`/`, `/product`,
`/roadmap`) loaded 139.3 kB of JS (gzip) at the first build, and an empty page of the pinned
next 16.4.2 and react 19.3.1 loads 132.0 kB, so the ceiling is 140 kB. Lowering it is a design
change; raising it needs a measurement and a judged change.

## Deploy

`.github/workflows/site.yml` builds and checks the site, then deploys `out/` to Cloudflare Pages,
project `swarmops`, whose production branch is `stable`: a push to `stable` is production
(swarmops.cc), a push to a `feat/**` branch a preview on its `pages.dev` address, and a pull
request is built and checked only. The deploy uses the repository secrets `CLOUDFLARE_API_TOKEN`
and `CLOUDFLARE_ACCOUNT_ID`; while either is unset, the deploy job leaves a notice and succeeds
without deploying. By hand, after `just web-build` and `just web-check`:

```sh
npx wrangler pages deploy web/out --project-name swarmops --branch preview   # a preview
npx wrangler pages deploy web/out --project-name swarmops --branch stable    # production
```

`public/_redirects` sends the addresses of the first site's guides (`/guides/<name>`) to their
docs pages (`/docs/guides/<name>`) with a 308. `/explainers/*` keeps its own policy in
`public/_headers` (its pages inline their script and style), and postbuild leaves those pages
without a meta policy. `public/install.sh` is the tracked copy of `../scripts/install.sh`:
`just check` (scripts/test-install.py) fails when the two differ.
