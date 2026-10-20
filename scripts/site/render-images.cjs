// Renders the website's raster images from their SVG sources:
//   site/favicon.svg        -> site/favicon.ico (16, 32, 48 px)
//   docs/assets/logo.svg    -> site/icon-192.png, site/icon-512.png,
//                              site/apple-touch-icon.png, site/icon-maskable-512.png
//   docs/assets/social-card.svg -> docs/assets/social-card.png, site/og-image.png
//
// It needs @resvg/resvg-js, which the repository does not vendor. Install it anywhere and
// point RESVG_JS at the package directory, for example:
//   npm install --prefix /tmp/resvg @resvg/resvg-js@2.6.2
//   RESVG_JS=/tmp/resvg/node_modules/@resvg/resvg-js node scripts/site/render-images.cjs
// Text in the social card uses the system's DejaVu Sans.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const { Resvg } = require(process.env.RESVG_JS || "@resvg/resvg-js");

const ROOT = path.resolve(__dirname, "..", "..");
const read = (p) => fs.readFileSync(path.join(ROOT, p), "utf8");
const write = (p, data) => {
  fs.writeFileSync(path.join(ROOT, p), data);
  console.log(`${p}  ${data.length} bytes`);
};
const BG = "#0d1117";

function png(svg, width, opts = {}) {
  return new Resvg(svg, {
    fitTo: { mode: "width", value: width },
    background: opts.background,
    font: { loadSystemFonts: true, defaultFontFamily: "DejaVu Sans" },
  }).render().asPng();
}

// A square canvas with the logo centred at `scale` of its side, on `bg` (or transparent).
function framed(svg, side, scale, bg) {
  const inner = svg.replace(/^<svg[^>]*>/, "").replace(/<\/svg>\s*$/, "");
  const view = /viewBox="([^"]+)"/.exec(svg)[1].split(/\s+/).map(Number);
  const size = view[2];
  const s = (side * scale) / size;
  const off = (side - size * s) / 2;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${side} ${side}" width="${side}" height="${side}">` +
    (bg ? `<rect width="${side}" height="${side}" fill="${bg}"/>` : "") +
    `<g transform="translate(${off} ${off}) scale(${s})">${inner}</g></svg>`;
}

// An ICO whose entries are PNGs (Windows Vista and later, every current browser).
function ico(images) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  const dir = Buffer.alloc(16 * images.length);
  let offset = 6 + dir.length;
  images.forEach(({ size, data }, i) => {
    const e = i * 16;
    dir.writeUInt8(size >= 256 ? 0 : size, e);
    dir.writeUInt8(size >= 256 ? 0 : size, e + 1);
    dir.writeUInt8(0, e + 2);
    dir.writeUInt8(0, e + 3);
    dir.writeUInt16LE(1, e + 4);
    dir.writeUInt16LE(32, e + 6);
    dir.writeUInt32LE(data.length, e + 8);
    dir.writeUInt32LE(offset, e + 12);
    offset += data.length;
  });
  return Buffer.concat([header, dir, ...images.map((i) => i.data)]);
}

// At 16 px the favicon's thin branches blur together, so the ICO's 16 px entry is a bolder
// glyph of the same mark: the cell, one fork of white branches and the amber main line.
const FAVICON_16 = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <path d="M32 3.5 56.7 17.75v28.5L32 60.5 7.3 46.25v-28.5z" fill="#7c5cff" stroke="#7c5cff" stroke-width="3" stroke-linejoin="round"/>
  <path d="M17 17c0 11 6 17 15 17M47 17c0 11-6 17-15 17" fill="none" stroke="#fff" stroke-width="8" stroke-linecap="round"/>
  <path d="M32 30v18" stroke="#ffc861" stroke-width="10" stroke-linecap="round"/>
</svg>`;
const favicon = read("site/favicon.svg");
write("site/favicon.ico", ico([
  { size: 16, data: png(FAVICON_16, 16) },
  ...[32, 48].map((size) => ({ size, data: png(favicon, size) })),
]));

const logo = read("docs/assets/logo.svg");
write("site/icon-192.png", png(framed(logo, 192, 0.92), 192));
write("site/icon-512.png", png(framed(logo, 512, 0.92), 512));
write("site/apple-touch-icon.png", png(framed(logo, 180, 0.74, BG), 180));
write("site/icon-maskable-512.png", png(framed(logo, 512, 0.62, BG), 512));

const card = png(read("docs/assets/social-card.svg"), 1280);
write("docs/assets/social-card.png", card);
write("site/og-image.png", card);
