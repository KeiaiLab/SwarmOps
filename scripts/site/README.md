# swarmops.cc

The project website is the Next.js site in [`web/`](../../web/README.md): its pages, build, checks
and deploy are described there. This directory holds the generators of the static files it
serves from [`web/public/`](../../web/public), which are committed as they are:

| Path | What it is |
| --- | --- |
| `web/public/explainers/store.html`, `store-light.html` | Fig. 1, the store as a machine: an interactive isometric figure, dark and light. |
| `web/public/explainers/store.svg`, `store-light.svg` | The same figure at rest (and `docs/assets/store-*.svg` for the README). |
| `web/public/THIRD-PARTY-NOTICES.txt` | The licence of Anatomy, whose kit the figure files inline; the figure's build writes it. |
| `web/public/favicon.*`, `web/public/*icon*.png`, `web/public/site.webmanifest` | Icons, from `web/public/favicon.svg` and `docs/assets/logo.svg`. |
| `web/public/og-image.png` | The social preview, from `docs/assets/social-card.svg` (also `docs/assets/social-card.png`, for GitHub's social preview). |

The figure pages carry their own content security policy (`/explainers/*` in
`web/public/_headers`), since they inline their script and style.

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
figure files and `web/public/THIRD-PARTY-NOTICES.txt` rebuild byte for byte. The interactive pages
inline the kit's script and stylesheet, and the stills its stylesheet, so each of them carries
Anatomy's licence in a comment, copied from that `LICENSE`, and `web/public/THIRD-PARTY-NOTICES.txt`
gives it once more. To move the pin, change `ANATOMY_COMMIT` and the sums in `PINNED` in
`store.mjs` together. The figure's line-end check needs Chrome:

```sh
git clone https://github.com/wheresryan22/anatomy /tmp/anatomy
git -C /tmp/anatomy checkout 1482e8469b0a9e0d8cc18ea2740a01423d29ae80
ANATOMY_DIR=/tmp/anatomy node scripts/site/explainers/store.mjs
python3 -m http.server 8000 --directory web/public &   # serves the figure pages
CHROME_PATH=<chrome> node /tmp/anatomy/scripts/lines.mjs http://127.0.0.1:8000/explainers/store.html   # line ends: 0
```

Every number the figure and the landing page show comes from [docs/perf.md](../../docs/perf.md)
or the README; change them together.

## Deploy

The site built in `web/` is deployed to Cloudflare Pages, project `swarmops`, with
`swarmops.cc` as its custom domain (the domain is on Cloudflare; its mail records stay as they
are); [`web/README.md`](../../web/README.md) describes the build. Every `*.pages.dev` address
answers with `X-Robots-Tag: noindex`, so only `swarmops.cc` is indexed. Set the repository's
social preview to `docs/assets/social-card.png` and its website to `https://swarmops.cc`.
