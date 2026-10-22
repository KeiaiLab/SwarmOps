// Fig. 1 of swarmops.cc: the native store as a machine, drawn with Anatomy
// (MIT, https://github.com/wheresryan22/anatomy), whose kit this script imports.
//
//   git clone https://github.com/wheresryan22/anatomy /tmp/anatomy
//   git -C /tmp/anatomy checkout 1482e8469b0a9e0d8cc18ea2740a01423d29ae80
//   ANATOMY_DIR=/tmp/anatomy node scripts/site/explainers/store.mjs
//
// Writes web/public/explainers/store.html and store-light.html (interactive, dark and light),
// web/public/explainers/store.svg and store-light.svg (stills), the same stills as
// docs/assets/store-dark.svg and store-light.svg for the README, and
// web/public/THIRD-PARTY-NOTICES.txt.
//
// The pages inline the kit's script and stylesheet, and the stills its stylesheet, so each of
// those files carries Anatomy's licence in a comment, copied from the checkout's LICENSE. The
// build reads only the files pinned below, and refuses a checkout whose copies differ from the
// pinned commit's, so every output reproduces byte for byte.
//
// The mapping, one part per idea of the storage spec:
//   commit cartridges on the feed rail  - writers' metadata transactions waiting for the lock;
//                                         the kernel wakes them in no strict order, so now and
//                                         then one hops to the head of the line
//   the lifting gate                    - meta/lock, one flock for every writer
//   the press stamping the paper tape   - one CRC-framed frame appended to journal.log
//   the tape folding into the tray      - the journal, one frame after another
//   pellets dropping into 16 bins       - object files into fan-out directories, no lock
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ANATOMY = process.env.ANATOMY_DIR;
if (!ANATOMY) {
  console.error("set ANATOMY_DIR to a checkout of https://github.com/wheresryan22/anatomy");
  process.exit(2);
}
// The Anatomy commit the outputs come from, and the sha256 of each of its files the build reads.
const ANATOMY_COMMIT = "1482e8469b0a9e0d8cc18ea2740a01423d29ae80";
const PINNED = {
  "kit/iso-kit.mjs": "6d3ecc725d30c3b0346da309cf61dfa5a65017c23620bbd4093009d4fb104018",
  "scripts/inline-kit.mjs": "01e08df7c2556ce52a7e8ed9009a332036b209ab7e42389874af2a65fda6e9db",
  LICENSE: "87c078ab9d90a3044f359e6b563d4e6afea7aa060897cf4f21618687b3856796",
};
for (const [file, sum] of Object.entries(PINNED)) {
  let bytes = null;
  try {
    bytes = readFileSync(join(ANATOMY, file));
  } catch {}
  if (!bytes || createHash("sha256").update(bytes).digest("hex") !== sum) {
    console.error(`ANATOMY_DIR's ${file} is not the one of anatomy ${ANATOMY_COMMIT}: check that commit out`);
    process.exit(2);
  }
}
const LICENSE = readFileSync(join(ANATOMY, "LICENSE"), "utf8").trimEnd();
const SOURCE = `Anatomy (https://github.com/wheresryan22/anatomy), commit ${ANATOMY_COMMIT}`;
// The notice goes into scripts, stylesheets and SVG text: it may close no comment and hold no markup.
if (/\*\/|[<&]/.test(LICENSE + SOURCE)) throw new Error("the licence cannot go into a comment as it is");
const SCRIPT_NOTICE = `/*! This script starts with kit/iso-kit.mjs, the kit of ${SOURCE}, under the licence below; the figure's own code follows it.\n\n${LICENSE}\n*/\n`;
const STYLE_NOTICE = `/*! This stylesheet comes from the kit of ${SOURCE}, under the licence below.\n\n${LICENSE}\n*/\n`;
// Puts the stylesheet's notice at the start of the stylesheet the kit's builders write: the first
// <style>, ahead of any script (the kit's script spells "<style>" too).
function withStyleNotice(markup) {
  const at = markup.indexOf("<style>") + "<style>".length;
  const script = markup.indexOf("<script");
  if (at < "<style>".length || (script >= 0 && script < at)) throw new Error("no stylesheet ahead of the script");
  return markup.slice(0, at) + STYLE_NOTICE + markup.slice(at);
}
const k = await import(pathToFileURL(join(ANATOMY, "kit/iso-kit.mjs")).href);
const { kitScript } = await import(pathToFileURL(join(ANATOMY, "scripts/inline-kit.mjs")).href);

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const WIDTH = 760;
const HEIGHT = 470;

// World units are about one pixel of the machine. x along the feed, y across it, z up.
const PLATE = { x: -236, y: -96, w: 470, d: 192, r: 10 };
const PLATE_H = 12;
const FOOT_H = 6;
const CHANNEL = { x: -214, y: -24, w: 274, d: 48, r: 3 };
const CH_TOP = 4;
const RAIL_H = 7;
const CART = { w: 22, d: 30, h: 16 };
const GATE_X = -40;
const PRESS_X = 12;
const HEAD_X = -66;
const SLOT = 26;
const QUEUE = 5;
const EXIT_X = 56;
const TAPE = { x: PRESS_X - 6, w: 12 };
const DECK = { x: PRESS_X - 10, y: -60, w: 20, d: 118, r: 2 };
const FRAME_LEN = 13;
const TRAY = { x: PRESS_X - 18, y: 58, w: 36, d: 30, r: 3 };
const SUPPLY = { x: PRESS_X - 18, y: -88, w: 36, d: 28, r: 3 };
const BINS = { x: 104, y: -66, w: 112, d: 112, r: 5 };
const BIN_H = 16;
const RAM_UP = 40;
const BEAM_Z = 66;

const P = k.fitProjection(
  [...k.boxCorners(PLATE, -PLATE_H - FOOT_H, 0), [PRESS_X, 0, BEAM_Z + 8], [GATE_X, 0, 54], [BINS.x + 56, 0, 70]],
  WIDTH,
  HEIGHT,
  { pad: 34, azimuth: 36 },
);
const at = (point) => k.iso(point, P);
const S = (paths, style) => k.solidSvg(paths, style);
const L = (d, style) => k.lineSvg(d, style);
const D = (points, style) => k.dotsSvg(points, style);
const byDepth = (points) => [...points].sort((a, b) => k.depthOf([a[0], a[1], 0], P) - k.depthOf([b[0], b[1], 0], P));
const cartPlan = (cx) => ({ x: cx - CART.w / 2, y: -CART.d / 2, w: CART.w, d: CART.d, r: 2 });

function plate() {
  const out = [`<path class="iso-halo" d="${k.haloOf(PLATE, -PLATE_H - FOOT_H, PLATE_H, P)}"/>`];
  for (const [x, y] of byDepth(k.corners(PLATE, 24))) {
    out.push(S(k.cylinder(x, y, 8, -PLATE_H - FOOT_H, 1.8, P, 28), { tone: "lo" }));
    out.push(S(k.cylinder(x, y, 6, -PLATE_H - FOOT_H + 1.8, FOOT_H - 1.8, P, 28), { tone: "mid" }));
  }
  out.push(S(k.slabOf(PLATE, -PLATE_H, PLATE_H, P, 8, 1.6), { tone: "mid" }));
  out.push(L(k.planOutline(k.insetPlan(PLATE, 6), 0, P), { tone: "lo" }));
  const screws = k.corners(PLATE, 11);
  out.push(L(screws.map(([x, y]) => k.ring(x, y, 2.4, 0, P, 16)).join(""), { tone: "lo" }));
  out.push(D(screws.map(([x, y]) => at([x, y, 0])), { size: 0.5 }));
  const strip = { x: -212, y: PLATE.y + PLATE.d - 16, w: 300, d: 8, r: 1 };
  out.push(S(k.slabOf(strip, 0, 0.6, P, 3), { tone: "lo", crease: "none" }));
  const ticks = k.topTicks(strip.x + 4, strip.x + strip.w - 4, 4, 5, strip.y + 0.8, 0.6, [2.2, 4], P);
  out.push(L(ticks.minor, { tone: "lo" }), L(ticks.major, { tone: "mid" }));
  out.push(D([at([strip.x + 2, strip.y + 4, 0.6]), at([strip.x + strip.w - 2, strip.y + 4, 0.6])], { size: 0.45 }));
  return out.join("");
}

function supply() {
  const out = [S(k.slabOf(SUPPLY, 0, 18, P, 6, 0.8), { tone: "mid" })];
  out.push(L(k.planOutline(k.insetPlan(SUPPLY, 3), 18, P), { tone: "faint" }));
  out.push(S(k.slabOf({ x: TAPE.x, y: SUPPLY.y + SUPPLY.d - 3, w: TAPE.w, d: 4, r: 0.5 }, 18, 0.6, P, 2), { tone: "hi", crease: "none" }));
  out.push(D([at([SUPPLY.x + 4, SUPPLY.y + 4, 18]), at([SUPPLY.x + SUPPLY.w - 4, SUPPLY.y + 4, 18])], { size: 0.45 }));
  return out.join("");
}

function deck() {
  const out = [S(k.slabOf(DECK, 0, CH_TOP, P, 3, 0.4), { tone: "mid" })];
  out.push(S(k.slabOf({ x: TAPE.x, y: DECK.y, w: TAPE.w, d: DECK.d, r: 0.3 }, CH_TOP, 0.5, P, 2), { tone: "hi", crease: "none", flat: true }));
  out.push(L(k.lineOnTop([TAPE.x, DECK.y], [TAPE.x, DECK.y + DECK.d], CH_TOP + 0.5, P), { tone: "lo" }));
  out.push(L(k.lineOnTop([TAPE.x + TAPE.w, DECK.y], [TAPE.x + TAPE.w, DECK.y + DECK.d], CH_TOP + 0.5, P), { tone: "lo" }));
  return out.join("");
}

// One printed frame on the tape, drawn at y = 0 and moved along the tape.
function frameMark() {
  const z = CH_TOP + 0.55;
  const box = [[TAPE.x + 1.6, -FRAME_LEN / 2 + 1.5], [TAPE.x + TAPE.w - 1.6, -FRAME_LEN / 2 + 1.5], [TAPE.x + TAPE.w - 1.6, FRAME_LEN / 2 - 1.5], [TAPE.x + 1.6, FRAME_LEN / 2 - 1.5]];
  const rows = [-2.2, 0, 2.2].map((dy) => k.lineOnTop([TAPE.x + 3, dy], [TAPE.x + TAPE.w - 5.5, dy], z, P)).join("");
  return L(k.onTop(box, z, P, true), { tone: "mid" }) + L(rows, { tone: "lo", free: true }) + D([at([TAPE.x + TAPE.w - 3.4, 2.2, z])], { size: 0.55, tone: "hi" });
}

function tray(stack) {
  const out = [S(k.slabOf(TRAY, 0, 10, P, 6, 0.8), { tone: "mid" })];
  out.push(L(k.planOutline(k.insetPlan(TRAY, 3), 10, P), { tone: "lo" }));
  out.push(`<g data-part="stack">${S(k.slabOf({ x: TAPE.x, y: TRAY.y + 4, w: TAPE.w, d: TRAY.d - 8, r: 0.5 }, 10, stack, P, 2), { tone: "hi", crease: "lo" })}</g>`);
  return out.join("");
}

function channel() {
  const out = [S(k.slabOf(CHANNEL, 0, CH_TOP, P, 4, 0.5), { tone: "mid" })];
  out.push(L(k.lineOnTop([CHANNEL.x, 0], [CHANNEL.x + CHANNEL.w, 0], CH_TOP, P), { tone: "faint" }));
  const ticks = k.topTicks(CHANNEL.x + 6, GATE_X - 8, 4, 5, CHANNEL.y + CHANNEL.d - 9, CH_TOP, [1.6, 3], P);
  out.push(L(ticks.minor, { tone: "faint" }), L(ticks.major, { tone: "lo" }));
  return out.join("");
}

function rail(y) {
  const plan = { x: CHANNEL.x, y, w: CHANNEL.w, d: 5, r: 1 };
  return S(k.slabOf(plan, CH_TOP, RAIL_H, P, 3, 0.4), { tone: "mid" }) + D([CHANNEL.x + 7, GATE_X - 12, EXIT_X - 4].map((x) => at([x, y + 2.5, CH_TOP + RAIL_H])), { size: 0.42 });
}

function cartridge(lit) {
  const plan = cartPlan(0);
  const out = [S(k.slabOf(plan, CH_TOP, CART.h, P, 4, 0.8), { tone: lit ? "hi" : "mid", crease: "lo", lit })];
  const top = CH_TOP + CART.h;
  out.push(L(k.planOutline(k.insetPlan(plan, 2.6), top, P, 3), { tone: "faint" }));
  out.push(L([-6, 0, 6].map((dy) => k.lineOnTop([-6, dy], [6, dy], top, P)).join(""), { tone: lit ? "mid" : "lo", free: true }));
  out.push(D([at([plan.x + 2.4, plan.y + 2.4, top]), at([plan.x + plan.w - 2.4, plan.y + plan.d - 2.4, top])], { size: 0.4, tone: "lo" }));
  return out.join("");
}

function gatePost(y) {
  return S(k.cylinder(GATE_X, y, 3.2, 0, 50, P, 24), { tone: "mid" }) + S(k.cylinder(GATE_X, y, 4.2, 50, 2.4, P, 24), { tone: "mid" });
}

function gateBar() {
  const bar = { x: GATE_X - 4, y: -31, w: 8, d: 62, r: 1.5 };
  const out = [S(k.slabOf(bar, 52.4, 5, P, 3, 0.5), { tone: "mid" })];
  out.push(k.faceTextSvg(k.topMatrix([GATE_X - 0.6, -14], 57.4, P, "y"), "meta/lock", { size: 3.2, tone: "lo" }));
  out.push(D([at([GATE_X, -27, 57.4]), at([GATE_X, 27, 57.4])], { size: 0.45 }));
  return out.join("");
}

function gatePlate() {
  const plan = { x: GATE_X - 2.5, y: -21, w: 5, d: 42, r: 1 };
  const out = [S(k.slabOf(plan, CH_TOP + 0.6, 24, P, 3, 0.4), { tone: "hi", crease: "lo" })];
  out.push(L(k.planOutline(k.insetPlan(plan, 1), CH_TOP + 24.6, P, 2), { tone: "faint" }));
  out.push(D([at([GATE_X, -17, CH_TOP + 24.6]), at([GATE_X, 17, CH_TOP + 24.6])], { size: 0.4 }));
  return out.join("");
}

function pressPost(y) {
  const plan = { x: PRESS_X - 4, y: y - 4, w: 8, d: 8, r: 1.5 };
  return S(k.slabOf(plan, 0, BEAM_Z, P, 3, 0.5), { tone: "mid" }) + L(k.sideSeam(plan, 20, P, 3), { tone: "faint" });
}

function beam() {
  const plan = { x: PRESS_X - 9, y: -38, w: 18, d: 76, r: 2.5 };
  const out = [S(k.slabOf(plan, BEAM_Z, 8, P, 4, 0.8), { tone: "mid" })];
  out.push(L(k.planOutline(k.insetPlan(plan, 3), BEAM_Z + 8, P, 3), { tone: "faint" }));
  out.push(D([[PRESS_X - 5, -33], [PRESS_X + 5, -33], [PRESS_X - 5, 33], [PRESS_X + 5, 33]].map(([x, y]) => at([x, y, BEAM_Z + 8])), { size: 0.45 }));
  out.push(S(k.cylinder(PRESS_X, 0, 6.5, BEAM_Z + 8, 4, P, 28), { tone: "mid" }));
  out.push(L(k.knurl(PRESS_X, 0, 6.5, BEAM_Z + 8, BEAM_Z + 12, 30, P), { tone: "lo" }));
  return out.join("");
}

// The ram, built with its foot at z = 0 and lifted with translateAlong.
function ram() {
  const out = [S(k.cylinder(PRESS_X, 0, 7, 0, 3, P, 28), { tone: "hi", crease: "lo" })];
  out.push(S(k.cylinder(PRESS_X, 0, 3.4, 3, 30, P, 20), { tone: "mid", crease: "none" }));
  out.push(L([10, 20].map((z) => k.sideArc(PRESS_X, 0, 3.4, z, P, 12)).join(""), { tone: "faint" }));
  return out.join("");
}

function bins() {
  const out = [`<path class="iso-halo" d="${k.haloOf(BINS, 0, BIN_H, P)}"/>`, S(k.slabOf(BINS, 0, BIN_H, P, 8, 1), { tone: "mid" })];
  const cell = (BINS.w - 8) / 4;
  for (let i = 0; i < 4; i++) {
    for (let j = 0; j < 4; j++) {
      const plan = { x: BINS.x + 4 + i * cell + 2, y: BINS.y + 4 + j * cell + 2, w: cell - 4, d: cell - 4, r: 2 };
      out.push(L(k.planOutline(plan, BIN_H, P, 3), { tone: "lo" }));
      out.push(L(k.planOutline(k.insetPlan(plan, 3), BIN_H, P, 3), { tone: "faint" }));
    }
  }
  out.push(D(k.corners(BINS, 6).map(([x, y]) => at([x, y, BIN_H])), { size: 0.45 }));
  return out.join("");
}

// A pellet (an object file) built at its own place over a bin and moved in z.
const CELL = (BINS.w - 8) / 4;
const binCentre = (i, j) => [BINS.x + 4 + (i + 0.5) * CELL, BINS.y + 4 + (j + 0.5) * CELL];
function pellet([x, y]) {
  return S(k.cylinder(x, y, 3.6, 0, 3.4, P, 18), { tone: "hi", crease: "lo" });
}
// Where a pellet will land: a soft halo on the bin's top, faded in as the pellet nears it.
function landing([x, y]) {
  return `<path class="iso-halo" d="${k.cylinder(x, y, 5, BIN_H, 0.1, P, 18).fill}" style="filter:blur(2px)"/>`;
}
// The journal's frame counter: a housing with a window of six digits, read like an odometer.
const COUNTER = { x: -176, y: 44, w: 70, d: 30, r: 4 };
function counter() {
  const out = [`<path class="iso-halo" d="${k.haloOf(COUNTER, 0, 14, P)}"/>`, S(k.slabOf(COUNTER, 0, 14, P, 6, 1), { tone: "mid" })];
  const win = { x: COUNTER.x + 8, y: COUNTER.y + 8, w: COUNTER.w - 22, d: COUNTER.d - 16, r: 1.5 };
  out.push(S(k.slabOf(win, 14, 0.6, P, 3), { tone: "lo", crease: "none", flat: true }));
  for (let i = 1; i < 6; i++) out.push(L(k.lineOnTop([win.x + (win.w * i) / 6, win.y], [win.x + (win.w * i) / 6, win.y + win.d], 14.6, P), { tone: "faint" }));
  out.push(`<g data-part="counter">${k.faceTextSvg(k.topMatrix([win.x + 2.2, win.y + win.d / 2 + 2.2], 14.6, P, "x"), "000128", { size: 6.4, tone: "hi" })}</g>`);
  const knob = [COUNTER.x + COUNTER.w - 7, COUNTER.y + COUNTER.d / 2];
  out.push(S(k.cylinder(knob[0], knob[1], 4.2, 14, 4, P, 24), { tone: "mid" }));
  out.push(L(k.knurl(knob[0], knob[1], 4.2, 14, 18, 24, P), { tone: "lo" }));
  out.push(D(k.corners(COUNTER, 3.4).map(([x, y]) => at([x, y, 14])), { size: 0.45 }));
  return out.join("");
}
const PELLETS = [[0, 1], [2, 0], [3, 2], [1, 3], [2, 2], [0, 3], [3, 0], [1, 1]];

const STILL = { queue: [0, 1, 2, 3, 4], press: true, gate: 0, ram: 0 };
const restStack = 4;

function body() {
  const parts = [];
  parts.push(plate());
  parts.push(supply());
  parts.push(deck());
  // Printed frames: one group per visible frame, positioned by the page script.
  for (let i = 0; i < 6; i++) parts.push(`<g data-part="frame" data-i="${i}" transform="${k.translateAlong(P, [0, FRAME_LEN * (i + 1), 0])}">${frameMark()}</g>`);
  parts.push(tray(restStack));
  parts.push(bins());
  parts.push(channel());
  parts.push(counter());
  parts.push(rail(-24));
  parts.push(gatePost(-27));
  parts.push(pressPost(-30));
  // Queue cartridges, drawn farthest first; the page script moves them.
  for (let i = QUEUE - 1; i >= 0; i--) parts.push(`<g data-part="queued" data-i="${i}" transform="${k.translateAlong(P, [HEAD_X - SLOT * i, 0, 0])}">${cartridge(false)}</g>`);
  parts.push(`<g data-part="gate" transform="${k.translateAlong(P, [0, 0, 0])}">${gatePlate()}</g>`);
  parts.push(`<g data-part="active" transform="${k.translateAlong(P, [PRESS_X, 0, 0])}">${cartridge(true)}</g>`);
  parts.push(`<g data-part="ram" transform="${k.translateAlong(P, [0, 0, CH_TOP + CART.h + RAM_UP])}">${ram()}</g>`);
  parts.push(rail(19));
  parts.push(gatePost(27));
  parts.push(gateBar());
  parts.push(pressPost(30));
  parts.push(beam());
  for (const [i, j] of PELLETS) parts.push(`<g data-part="landing" style="opacity:0">${landing(binCentre(i, j))}</g>`);
  for (const [i, j] of PELLETS) parts.push(`<g data-part="pellet" transform="${k.translateAlong(P, [0, 0, 999])}">${pellet(binCentre(i, j))}</g>`);
  return parts;
}

const label = "The SwarmOps store drawn as a machine. Commit cartridges wait on a feed rail behind a gate, the one metadata lock, which lets one through at a time, in no strict order; a press stamps it as one frame onto a paper tape, the append-only journal, and the tape folds into a tray. Beside it, object pellets drop into sixteen bins with no gate at all.";

function stillSvg(theme) {
  const svg = k.figureSvg({ width: WIDTH, height: HEIGHT, label, body: body(), standalone: theme });
  // The stills show the machine at rest: no pellet in the air.
  return withStyleNotice(
    svg
      .replace(/<g data-part="pellet"[^>]*>(?:(?!<g data-part=).)*?<\/g><\/g>/g, "")
      .replace(/<g data-part="landing"[^>]*>.*?<\/g>/g, ""),
  );
}

const live = `
const P = ${JSON.stringify(P)};
const C = ${JSON.stringify({ HEAD_X, SLOT, QUEUE, PRESS_X, EXIT_X, GATE_X, CH_TOP, CART_H: CART.h, RAM_UP, FRAME_LEN, BIN_H })};
const stage = document.querySelector(".iso-stage");
const svg = stage.querySelector("svg");
const q = (sel) => [...svg.querySelectorAll(sel)];
const queued = q('[data-part="queued"]').sort((a, b) => a.dataset.i - b.dataset.i);
const active = svg.querySelector('[data-part="active"]');
const gate = svg.querySelector('[data-part="gate"]');
const ram = svg.querySelector('[data-part="ram"]');
const frames = q('[data-part="frame"]').sort((a, b) => a.dataset.i - b.dataset.i);
const pellets = q('[data-part="pellet"]');
const landings = q('[data-part="landing"]');
const digits = svg.querySelector('[data-part="counter"] text');
const readout = document.querySelector("[data-readout]");
const still = matchMedia("(prefers-reduced-motion: reduce)");
const ease = (t) => (t <= 0 ? 0 : t >= 1 ? 1 : t * t * (3 - 2 * t));
const span = (t, a, b) => ease((t - a) / (b - a));
const CYCLE = 1.7;
// At each release the kernel wakes every waiter and the first to run takes the lock: no strict
// order. So in some cycles, while the press runs, a waiter further back hops to the head of the
// line, over the ones it passes (HOPS gives the waiter, by its place in the line; 0 is none).
const HOPS = [0, 3, 0, 1, 4, 0, 2, 0];
const HOP_Z = C.CART_H + 8;
const layer = gate.parentNode;
let hopper = -1;
// Waiting cartridges are drawn farthest first, and the one hopping over them last.
function lift(k) {
  if (k === hopper) return;
  for (let i = C.QUEUE - 1; i >= 0; i--) if (i !== k) layer.insertBefore(queued[i], gate);
  if (k >= 0) layer.insertBefore(queued[k], gate);
  hopper = k;
}
let clock = 0, last = 0, frame = 0, visible = false, written = 0, objects = 0, speed = 1;
const drops = pellets.map((_, i) => ({ t: -i * 0.29, el: pellets[i], halo: landings[i] }));
const TOP = 46, FALL = 1.15;
function pose(t) {
  const n = Math.floor(t / CYCLE), f = (t / CYCLE) % 1;
  const open = span(f, 0.0, 0.08) * (1 - span(f, 0.3, 0.38));
  gate.setAttribute("transform", translateAlong(P, [0, 0, 22 * open]));
  const travel = span(f, 0.04, 0.3);
  active.setAttribute("transform", translateAlong(P, [C.HEAD_X + (C.PRESS_X - C.HEAD_X) * travel + (C.EXIT_X - C.PRESS_X) * span(f, 0.66, 0.86), 0, 0]));
  active.style.opacity = String(1 - span(f, 0.8, 0.9));
  const advance = span(f, 0.06, 0.3);
  const k = HOPS[n % HOPS.length];
  const hop = k > 0 && f >= 0.46;
  lift(hop ? k : -1);
  const up = hop ? span(f, 0.46, 0.54) * (1 - span(f, 0.8, 0.88)) : 0;
  const along = hop ? span(f, 0.52, 0.82) : 0;
  queued.forEach((el, i) => {
    const slot = C.HEAD_X - C.SLOT * i;
    let x = slot - C.SLOT + C.SLOT * advance, z = 0;
    if (hop && i < k) x = slot - C.SLOT * along;
    if (hop && i === k) { x = slot + C.SLOT * k * along; z = HOP_Z * up; }
    el.setAttribute("transform", translateAlong(P, [x, 0, z]));
    el.style.opacity = i === C.QUEUE - 1 ? String(advance) : "1";
  });
  const press = span(f, 0.34, 0.46) * (1 - span(f, 0.5, 0.62));
  ram.setAttribute("transform", translateAlong(P, [0, 0, C.CH_TOP + C.CART_H + C.RAM_UP * (1 - press)]));
  const roll = span(f, 0.58, 0.72);
  const stamped = f >= 0.46 ? 1 : 0;
  frames.forEach((el, i) => {
    const y = C.FRAME_LEN * (i + roll) + (stamped ? 0 : C.FRAME_LEN);
    el.setAttribute("transform", translateAlong(P, [0, y, 0]));
    el.style.opacity = i === 0 ? String(stamped) : String(1 - span(y, 58, 70));
  });
  return n + stamped;
}
function tick(now) {
  const dt = last ? Math.min((now - last) / 1000, 1 / 30) : 1 / 60;
  last = now;
  clock += dt * speed;
  written = 128 + pose(clock);
  for (const d of drops) {
    d.t += dt * speed;
    const life = d.t % (FALL + 0.9);
    if (d.t < 0) { d.el.style.opacity = "0"; d.halo.style.opacity = "0"; continue; }
    const z = TOP - (TOP - C.BIN_H) * Math.min(1, (life / FALL) ** 2);
    if (life >= FALL) { d.el.style.opacity = "0"; d.halo.style.opacity = "0"; if (!d.done) { objects++; d.done = true; } continue; }
    d.done = false;
    d.el.style.opacity = String(Math.min(1, life / 0.18) * (z > C.BIN_H + 2 ? 1 : (z - C.BIN_H) / 2));
    d.halo.style.opacity = String(0.25 + 0.75 * (1 - (z - C.BIN_H) / (TOP - C.BIN_H)));
    d.el.setAttribute("transform", translateAlong(P, [0, 0, z]));
  }
  const waiting = 4 + (((clock / CYCLE) % 1) < 0.06 ? 1 : 0);
  const text = "frame " + written + " · " + waiting + " waiting for meta/lock · " + (2048 + objects) + " objects, no lock";
  if (readout.textContent !== text) readout.textContent = text;
  const shown = String(written).padStart(6, "0");
  if (digits && digits.textContent !== shown) digits.textContent = shown;
  frame = visible ? requestAnimationFrame(tick) : 0;
}
const run = () => { if (!frame && visible && !still.matches) { last = 0; frame = requestAnimationFrame(tick); } };
new IntersectionObserver((entries) => { visible = entries[entries.length - 1].isIntersecting; if (visible) run(); }, { rootMargin: "120px 0px" }).observe(stage);
stage.addEventListener("pointerenter", () => { speed = 0.35; });
stage.addEventListener("pointerleave", () => { speed = 1; });
stage.addEventListener("keydown", (event) => {
  if (event.key !== " " && event.key !== "Enter") return;
  event.preventDefault();
  speed = speed ? 0 : 1;
  if (speed) run();
});
pose(0.5);
`;

// What the kit's page leaves out: a description, a canonical, social tags and icons for a search
// engine, a heading, and muted text that reads against its background.
const ORIGIN = "https://swarmops.cc";
const PAGE_TITLE = "The SwarmOps store, as a machine";
const PAGE_DESC = "Fig. 1 of SwarmOps: commits wait for one metadata lock and become frames of one journal, while object writes take no lock. An interactive drawing of the store, with the numbers measured on one host.";
const CANONICAL = `${ORIGIN}/explainers/store`; // the dark page stands for both themes
const HEAD_TAGS = [
  `<meta name="description" content="${PAGE_DESC}">`,
  `<link rel="canonical" href="${CANONICAL}">`,
  `<link rel="icon" href="/favicon.ico" sizes="any"><link rel="icon" href="/favicon.svg" type="image/svg+xml">`,
  `<link rel="apple-touch-icon" href="/apple-touch-icon.png">`,
  `<meta property="og:type" content="website"><meta property="og:site_name" content="SwarmOps">`,
  `<meta property="og:title" content="${PAGE_TITLE}"><meta property="og:description" content="${PAGE_DESC}">`,
  `<meta property="og:url" content="${CANONICAL}">`,
  `<meta property="og:image" content="${ORIGIN}/og-image.png"><meta property="og:image:width" content="1280"><meta property="og:image:height" content="640">`,
  `<meta name="twitter:card" content="summary_large_image">`,
].join("");
// Muted text at 4.5:1 or better on both pages' backgrounds (the kit's #6e6e6e and #8c8c90 reach 3.0 to 3.9).
const HEAD_CSS = [
  `.iso{--anatomy-muted:#8c8c8c}`,
  `.iso[data-theme="light"]{--anatomy-muted:#666666}`,
  `.iso-title{margin:0 0 12px;font:600 15px/1.4 ui-sans-serif,system-ui,sans-serif;color:var(--anatomy-strong);text-align:center}`,
  `.iso-credit{text-align:center;font-size:12px;color:var(--anatomy-muted)}`,
  `.iso-credit a{color:var(--anatomy-text)}`,
].join("");
// Adds the head tags and a second stylesheet after the kit's. The kit's script spells "</style></head>"
// too, so only the first match, ahead of the script, is the page's.
function withHead(markup) {
  const at = markup.indexOf("</style></head>");
  const script = markup.indexOf("<script");
  if (at < 0 || (script >= 0 && script < at)) throw new Error("no head ahead of the script");
  return markup.slice(0, at) + "</style><style>" + HEAD_CSS + "</style>" + HEAD_TAGS + markup.slice(at + "</style>".length);
}

function page(theme) {
  const svg = k.figureSvg({ width: WIDTH, height: HEIGHT, label, body: body() });
  const plate = k.plateHtml({
    fig: "Fig 1",
    title: "The native store",
    hint: "Hover to slow down · Space pauses",
    readout: "frame 128 · 4 waiting for meta/lock · 2048 objects, no lock",
    keys: [
      { mark: "lit", label: "The commit in flight: the one writer holding meta/lock" },
      { mark: "raised", label: "Commits waiting for the lock, in no strict order, and objects dropping into fan-out bins with no lock" },
    ],
    caption:
      "Writers' metadata transactions wait for one lock, an flock: at each release the kernel wakes every waiter and the first to run takes it, so the order is not strict and a later writer can go first. Each transaction becomes one CRC-framed frame appended to journal.log; objects are made durable before any frame names them, into 256 fan-out directories, without a lock (sixteen bins drawn). Measured on one busy host (docs/perf.md, 2026-10-16, with the writer lock that ships): with 32 processes committing through the binary at once, 72.7 commits a second and a median commit of 427 ms (p99 656 ms), about 406 ms of it the wait for the lock; the metadata transactions alone, at store level, 120.9 a second; and object writes on 32 threads in one process 13.8× as fast as on one thread, as the disk and ext4's journal commit concurrent fsyncs together.",
    body: `<div class="iso-stage" tabindex="0" role="img" aria-label="${label}">${svg}</div>`,
  });
  const other = theme === "light" ? `<a href="/explainers/store">Dark version</a>` : `<a href="/explainers/store-light">Light version</a>`;
  const credit = `<p class="iso-credit">Drawn with <a href="https://github.com/wheresryan22/anatomy">Anatomy</a> (MIT, <a href="/THIRD-PARTY-NOTICES.txt">notice</a>) · ${other} · <a href="/">swarmops.cc</a></p>`;
  const title = theme === "light" ? `${PAGE_TITLE} (light version)` : PAGE_TITLE;
  const heading = `<h1 class="iso-title">${PAGE_TITLE}</h1>`;
  return withHead(withStyleNotice(k.pageHtml({ title, theme, body: heading + plate + credit, script: SCRIPT_NOTICE + kitScript() + live, width: 820 })));
}

const notices = `Third-party notices: swarmops.cc

Fig. 1, the store as a machine, is drawn with Anatomy, https://github.com/wheresryan22/anatomy,
at commit ${ANATOMY_COMMIT}. The interactive pages
/explainers/store.html and /explainers/store-light.html inline its kit's script
(kit/iso-kit.mjs) and stylesheet, and the stills /explainers/store.svg and
/explainers/store-light.svg its stylesheet (the repository's docs/assets/store-dark.svg and
store-light.svg are the same stills). Each of those files carries this licence in a comment too.
scripts/site/explainers/store.mjs, in the SwarmOps repository, builds them.

${LICENSE}
`;

const out = (rel, text) => {
  const p = join(ROOT, rel);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, text);
  console.log(rel, text.length);
};
out("web/public/explainers/store.html", page("dark"));
out("web/public/explainers/store-light.html", page("light"));
out("web/public/explainers/store.svg", stillSvg("dark"));
out("web/public/explainers/store-light.svg", stillSvg("light"));
out("docs/assets/store-dark.svg", stillSvg("dark"));
out("docs/assets/store-light.svg", stillSvg("light"));
out("web/public/THIRD-PARTY-NOTICES.txt", notices);
