// File: browser-extension/build.mjs
//
// Purpose:
// The one build script of the extension. Type-checks the sources (`tsc --noEmit`; a type error
// aborts), bundles them with esbuild and lays out a loadable unpacked extension.
//
// Usage:
//   node build.mjs --out <dir>   extension into <dir> (the app's build.rs passes OUT_DIR/ext-staging)
//   node build.mjs --tests       type-check src + test, bundle test/*.test.ts into .test-out/
//
// Output layout (--out):
//   manifest.json, icons/*.png, background.js (ESM service worker),
//   modules/<module>/driver.js for every src/modules/<module>/content/driver.ts (IIFE content
//   script, injected programmatically by its module).
//
// Notes:
// - The output directory is wiped only when it is empty or carries the `.ms-ext-out` marker
//   this script wrote there before, so a mistyped --out never deletes foreign files.
// - The output never contains the app-owned names `bridge.json`, `build-info.json` or
//   `ms-extension-stamp.json`; build.rs adds `build-info.json` itself.
// - No network, no global tools: tsc and esbuild come from node_modules (`npm ci`).

import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const ROOT = dirname(fileURLToPath(import.meta.url));
const OUT_MARKER = ".ms-ext-out";
const RESERVED_OUTPUT_NAMES = new Set(["bridge.json", "build-info.json", "ms-extension-stamp.json"]);
const TARGET = "chrome116";

class BuildError extends Error {}

function parseArgs(argv) {
  const args = { out: null, tests: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--tests") args.tests = true;
    else if (arg === "--out") {
      const value = argv[i + 1];
      if (value === undefined) throw new BuildError("--out needs a directory");
      args.out = resolve(value);
      i += 1;
    } else throw new BuildError(`unknown argument '${arg}'`);
  }
  if (args.out === null && !args.tests) throw new BuildError("usage: node build.mjs --out <dir> | --tests");
  return args;
}

/** Runs the TypeScript compiler from node_modules (no npx, so it works the same on Windows). */
function typecheck(project) {
  const tsc = join(ROOT, "node_modules", "typescript", "bin", "tsc");
  const result = spawnSync(process.execPath, [tsc, "--noEmit", "-p", project], { cwd: ROOT, stdio: "inherit" });
  if (result.error) throw new BuildError(`could not run tsc: ${result.error.message}`);
  if (result.status !== 0) throw new BuildError(`type check failed (${project})`);
}

/** Empties `dir` if it is ours (marker) or empty; refuses anything else. */
function prepareOutDir(dir) {
  if (existsSync(dir)) {
    if (!statSync(dir).isDirectory()) throw new BuildError(`${dir} exists and is not a directory`);
    const entries = readdirSync(dir);
    if (entries.length > 0 && !entries.includes(OUT_MARKER)) {
      throw new BuildError(`${dir} is not empty and was not created by build.mjs; refusing to wipe it`);
    }
    rmSync(dir, { recursive: true, force: true });
  }
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, OUT_MARKER), "build.mjs output directory; wiped on every build\n");
}

/** `src/modules/<module>/content/driver.ts` entries, as [source, output-relative path]. */
function contentEntries() {
  const modulesDir = join(ROOT, "src", "modules");
  if (!existsSync(modulesDir)) return [];
  const entries = [];
  for (const name of readdirSync(modulesDir).sort()) {
    const driver = join(modulesDir, name, "content", "driver.ts");
    if (existsSync(driver)) entries.push([driver, join("modules", name, "driver.js")]);
  }
  return entries;
}

function listFiles(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) out.push(...listFiles(path));
    else out.push(path);
  }
  return out;
}

async function buildExtension(outDir) {
  typecheck(".");
  prepareOutDir(outDir);
  await build({
    entryPoints: [join(ROOT, "src", "background.ts")],
    outfile: join(outDir, "background.js"),
    bundle: true,
    format: "esm",
    platform: "browser",
    target: TARGET,
    logLevel: "warning",
    legalComments: "none",
  });
  for (const [source, output] of contentEntries()) {
    await build({
      entryPoints: [source],
      outfile: join(outDir, output),
      bundle: true,
      format: "iife",
      platform: "browser",
      target: TARGET,
      logLevel: "warning",
      legalComments: "none",
    });
  }
  copyFileSync(join(ROOT, "manifest.json"), join(outDir, "manifest.json"));
  mkdirSync(join(outDir, "icons"), { recursive: true });
  for (const icon of readdirSync(join(ROOT, "icons")).filter((n) => n.endsWith(".png")).sort()) {
    copyFileSync(join(ROOT, "icons", icon), join(outDir, "icons", icon));
  }
  for (const file of listFiles(outDir)) {
    const rel = relative(outDir, file).split(sep).join("/");
    if (RESERVED_OUTPUT_NAMES.has(rel)) throw new BuildError(`build produced the reserved file '${rel}'`);
  }
}

async function buildTests() {
  const testOut = join(ROOT, ".test-out");
  // The tests are type-checked with Node's types on top of the extension's own config; the
  // generated config lives in the (ignored) output directory so the tracked tsconfig.json
  // keeps `types: ["chrome"]` and src never sees Node globals.
  prepareOutDir(testOut);
  const testConfig = join(testOut, "tsconfig.test.json");
  writeFileSync(
    testConfig,
    JSON.stringify({ extends: "../tsconfig.json", compilerOptions: { types: ["chrome", "node"] }, include: ["../src/**/*.ts", "../test/**/*.ts"] }, null, 2),
  );
  typecheck(testConfig);
  const tests = readdirSync(join(ROOT, "test")).filter((n) => n.endsWith(".test.ts")).sort();
  if (tests.length === 0) throw new BuildError("no test/*.test.ts files found");
  for (const name of tests) {
    await build({
      entryPoints: [join(ROOT, "test", name)],
      outfile: join(testOut, name.replace(/\.ts$/, ".mjs")),
      bundle: true,
      format: "esm",
      platform: "node",
      target: "node20",
      packages: "external",
      logLevel: "warning",
    });
  }
}

try {
  const args = parseArgs(process.argv.slice(2));
  if (args.tests) await buildTests();
  if (args.out !== null) await buildExtension(args.out);
} catch (error) {
  console.error(`build.mjs: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
