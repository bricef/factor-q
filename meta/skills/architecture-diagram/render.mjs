#!/usr/bin/env node
// Render the factor-q architecture diagram: DOT -> SVG (-> PNG when a
// chromium is available). Prefers a real `dot` on PATH; falls back to the
// WASM Graphviz (@viz-js/viz), installed into this directory on first use.
//
// Usage: node meta/skills/architecture-diagram/render.mjs [dot-file]
//   dot-file defaults to docs/design/committed/architecture-diagram.dot (relative to
//   the CWD); the SVG and PNG are written next to it.

import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dotPath = resolve(process.argv[2] ?? "docs/design/committed/architecture-diagram.dot");
const svgPath = dotPath.replace(/\.dot$/, ".svg");
const pngPath = dotPath.replace(/\.dot$/, ".png");

const dot = readFileSync(dotPath, "utf8");
const svg = await renderSvg(dot);
writeFileSync(svgPath, svg);
console.log(`svg: ${svgPath} (${svg.length} bytes)`);
await renderPng(svgPath, pngPath);

/** Real graphviz if present (exact layout parity with CI images), else WASM. */
async function renderSvg(dot) {
  const viaDot = spawnSync("dot", ["-Tsvg"], {
    input: dot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (!viaDot.error && viaDot.status === 0) return viaDot.stdout;

  let mod;
  try {
    mod = await import("@viz-js/viz");
  } catch {
    console.error("no `dot` on PATH and @viz-js/viz not installed — installing (one-off)…");
    const npm = spawnSync("npm", ["install", "--no-fund", "--no-audit"], {
      cwd: here,
      stdio: "inherit",
    });
    if (npm.error || npm.status !== 0) {
      throw new Error("npm install failed and no `dot` on PATH — cannot render");
    }
    mod = await import("@viz-js/viz");
  }
  const viz = await mod.instance();
  return viz.renderString(dot, { format: "svg" });
}

/** Rasterise via a headless chromium; skip gracefully when none exists. */
async function renderPng(svgPath, pngPath) {
  const chrome = findChromium();
  if (!chrome) {
    console.error("no chromium found ($CHROME_BIN, PATH, ~/.cache/ms-playwright) — skipped PNG; the SVG is authoritative");
    return;
  }
  const m = readFileSync(svgPath, "utf8").match(
    /viewBox="[\d.]+ [\d.]+ ([\d.]+) ([\d.]+)"/,
  );
  if (!m) throw new Error(`no viewBox in ${svgPath}`);
  // SVG dimensions are pt; chromium renders at 4/3 px per pt. Oversize,
  // then trim to content below.
  const w = Math.ceil((+m[1] * 4) / 3) + 80;
  const h = Math.ceil((+m[2] * 4) / 3) + 80;

  const tmp = mkdtempSync(join(tmpdir(), "fq-arch-"));
  const shot = join(tmp, "shot.png");
  try {
    const run = spawnSync(
      chrome,
      [
        "--headless",
        "--no-sandbox",
        "--disable-gpu",
        "--hide-scrollbars",
        `--screenshot=${shot}`,
        `--window-size=${w},${h}`,
        "--default-background-color=FFFFFFFF",
        `file://${svgPath}`,
      ],
      { stdio: "ignore" },
    );
    if (run.error || !existsSync(shot)) {
      console.error("chromium screenshot failed — skipped PNG");
      return;
    }
    if (!trimWithImageMagick(shot, pngPath)) copyFileSync(shot, pngPath);
    console.log(`png: ${pngPath}`);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}

/** Trim to content + uniform white border. IM7 is `magick`, IM6 `convert`. */
function trimWithImageMagick(src, dest) {
  const args = [src, "-trim", "+repage", "-bordercolor", "white", "-border", "24", dest];
  for (const bin of ["magick", "convert"]) {
    const r = spawnSync(bin, args, { stdio: "ignore" });
    if (!r.error && r.status === 0 && existsSync(dest)) return true;
  }
  return false;
}

function findChromium() {
  const fromEnv = process.env.CHROME_BIN;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  for (const c of ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"]) {
    if (!spawnSync(c, ["--version"], { stdio: "ignore" }).error) return c;
  }
  const cache = join(homedir(), ".cache", "ms-playwright");
  if (existsSync(cache)) {
    const dirs = readdirSync(cache)
      .filter((d) => d.startsWith("chromium-"))
      .sort()
      .reverse();
    for (const d of dirs) {
      for (const sub of ["chrome-linux64/chrome", "chrome-linux/chrome"]) {
        const p = join(cache, d, sub);
        if (existsSync(p)) return p;
      }
    }
  }
  return null;
}
