# swarmops.cc

The project website: a static site with no build step, no web fonts, no scripts on the main page,
and no tracking. Everything it serves is in [`site/`](../../site).

| Path | What it is |
| --- | --- |
| `site/index.html`, `site/styles.css` | The landing page; dark by default, light by the reader's preference. |
| `site/explainers/store.html`, `store-light.html` | Fig. 1, the store as a machine: an interactive isometric figure, dark and light. |
| `site/explainers/store.svg`, `store-light.svg` | The same figure at rest, for the landing page (and `docs/assets/store-*.svg` for the README). |
| `site/THIRD-PARTY-NOTICES.txt` | The licence of Anatomy, whose kit the figure files inline; the figure's build writes it. |
| `site/favicon.*`, `site/*icon*.png`, `site/site.webmanifest` | Icons, from `site/favicon.svg` and `docs/assets/logo.svg`. |
| `site/og-image.png` | The social preview, from `docs/assets/social-card.svg` (also `docs/assets/social-card.png`, for GitHub's social preview). |
| `site/_headers` | Cloudflare Pages response headers: a strict content security policy, HSTS and caching. |

## Preview

```sh
python3 -m http.server 8000 --directory site    # then open http://127.0.0.1:8000/
```

The pages use root-relative paths, so open them through a server, not as files.

## Regenerate the images

The PNGs and the ICO are rendered from their SVG sources with resvg, which the repository does
not vendor:

```sh
npm install --prefix /tmp/resvg @resvg/resvg-js@2.6.2
RESVG_JS=/tmp/resvg/node_modules/@resvg/resvg-js node scripts/site/render-images.cjs
```

The figure is built with [Anatomy](https://github.com/wheresryan22/anatomy) (MIT), whose kit
the build script imports, pinned at commit `1482e84`: the script reads only the kit's two files
and its `LICENSE`, and refuses a checkout whose copies differ from that commit's, so the six
figure files and `site/THIRD-PARTY-NOTICES.txt` rebuild byte for byte. The interactive pages
inline the kit's script and stylesheet, and the stills its stylesheet, so each of them carries
Anatomy's licence in a comment, copied from that `LICENSE`, and `site/THIRD-PARTY-NOTICES.txt`
gives it once more. To move the pin, change `ANATOMY_COMMIT` and the sums in `PINNED` in
`store.mjs` together. The figure's line-end check needs Chrome:

```sh
git clone https://github.com/wheresryan22/anatomy /tmp/anatomy
git -C /tmp/anatomy checkout 1482e8469b0a9e0d8cc18ea2740a01423d29ae80
ANATOMY_DIR=/tmp/anatomy node scripts/site/explainers/store.mjs
CHROME_PATH=<chrome> node /tmp/anatomy/scripts/lines.mjs http://127.0.0.1:8000/explainers/store.html   # line ends: 0
```

Every number the figure and the landing page show comes from [docs/perf.md](../../docs/perf.md)
or the README; change them together.

## Deploy

**Cloudflare Pages** (the domain is on Cloudflare): create a Pages project connected to the
repository, with production branch `stable`, no build command and build output directory
`site`, then add `swarmops.cc` as its custom domain (Cloudflare writes the DNS record; leave
the domain's mail records as they are). A direct upload works without connecting the
repository:

```sh
npx wrangler pages deploy site --project-name swarmops --branch stable
```

**GitHub Pages** serves a private repository only on paid plans; once the repository is public,
an Actions workflow can publish the same `site/` directory. After the switch, set the
repository's social preview to `docs/assets/social-card.png` and its website to
`https://swarmops.cc`.
