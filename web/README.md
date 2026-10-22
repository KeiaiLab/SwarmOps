# swarmops.cc, the website

A static site, built with Next.js 16 (`output: "export"`), Fumadocs for `/docs`, Tailwind 4 and
shadcn/ui components. Every page is HTML in `out/`; there is no server runtime. Production still
serves `site/` until the deploy moves to this directory.

```sh
just web-install   # npm ci: the only step that needs the network
just web-build     # next build, then scripts/postbuild.mjs; writes out/
just web-check     # scripts/check-site.mjs over out/
```

`npm run dev` serves the site with live reload on port 3000.

## Where pages come from

| Route | Source |
| --- | --- |
| `/` | `app/(product)/page.tsx` |
| `/product/**` | `content/product/**/*.md`, front matter `title` and `description` |
| `/roadmap` | `content/roadmap.md`, same front matter |
| `/docs/**` | `../docs/**/*.md` and `../CHANGELOG.md`, as they are in the repository |

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

`scripts/postbuild.mjs` copies `../scripts/install.sh` to `out/install.sh` (get.swarmops.cc
serves it), the icons and `/explainers/` from `../site/`, and `../docs/assets/`. It then writes a
`<meta http-equiv="Content-Security-Policy">` into every page, which allows that page's inline
scripts by sha256 and no other inline script. `public/_headers` keeps the site-wide headers
(HSTS, `frame-ancestors 'none'`, the noindex rules for pages.dev and get.swarmops.cc); its
looser `script-src` lets the per-page policy bind.

`scripts/check-site.mjs` fails on a missing docs page, a broken link or anchor, a link inside a
link, a script, stylesheet or font from another origin, an inline script the page's policy does
not allow, product pages over the first-load JS budget, or a missing served file.
