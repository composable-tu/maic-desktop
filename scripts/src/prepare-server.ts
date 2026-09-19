// prepare-server — build openmaic-src (read-only) and stage the Tauri sidecar payload.
// Never modifies files inside openmaic-src/; it only runs its install/build and copies outputs.
//
// Source lives in scripts/src/ (TypeScript) and is bundled by tsdown to
// scripts/dist/prepare-server.mjs — two levels below the repo root, which is
// where all paths below are resolved from.
//
// Usage:
//   pnpm prepare:server [-- <args>]
//   node scripts/dist/prepare-server.mjs [--target <rust-triple>] [--skip-build] [--skip-node]
//
// Output:
//   src-tauri/resources/server/   — standalone server.js + .next/static + public
//   src-tauri/binaries/openmaic-node-<triple>[.exe] — downloaded Node 22 LTS binary
//   src-tauri/resources/server/.build-meta.json — provenance record

import { execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { assertNoLinks, copyDir, findLinks, materializeLinks } from './links';
import { resolveNode22Latest, stageNodeBinary } from './node-runtime';
import { smokeTestBundledTarball } from './smoke';
import { pathExists, run } from './shell';
import { hostTriple } from './triples';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..');
const srcDir = path.join(root, 'openmaic-src');
const resourcesDir = path.join(root, 'src-tauri', 'resources', 'server');
const binariesDir = path.join(root, 'src-tauri', 'binaries');

interface Args {
  target: string | null;
  skipBuild: boolean;
  skipNode: boolean;
}

function parseArgs(argv: string[]): Args {
  const out: Args = { target: null, skipBuild: false, skipNode: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === '--target') out.target = argv[++i] ?? '';
    else if (a === '--skip-build') out.skipBuild = true;
    else if (a === '--skip-node') out.skipNode = true;
    else throw new Error(`unknown arg: ${a}`);
  }
  return out;
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  const triple = args.target || hostTriple();
  console.log(`prepare-server: target=${triple}`);

  if (!(await pathExists(path.join(srcDir, 'package.json')))) {
    throw new Error(`submodule not checked out: ${srcDir} (run: git submodule update --init --recursive)`);
  }

  if (!args.skipBuild) {
    // Hoisted (npm-style) node_modules: no .pnpm symlinks/junctions for the
    // server's own deps, so the staged tree can be made link-free and the
    // bundle then behaves the same on macOS, Linux and Windows.
    run('pnpm', ['--dir', srcDir, 'install', '--frozen-lockfile', '--config.node-linker=hoisted']);
    run('pnpm', ['--dir', srcDir, 'build']);
  }

  const standaloneDir = path.join(srcDir, '.next', 'standalone');
  const staticDir = path.join(srcDir, '.next', 'static');
  const publicDir = path.join(srcDir, 'public');
  if (!(await pathExists(path.join(standaloneDir, 'server.js')))) {
    throw new Error(`standalone server not found at ${standaloneDir}/server.js — build may have failed`);
  }

  console.log('staging standalone server into resources/server/');
  await fs.rm(resourcesDir, { recursive: true, force: true });
  await fs.mkdir(resourcesDir, { recursive: true });
  await copyDir(standaloneDir, resourcesDir);
  // Dockerfile parity: standalone output expects .next/static and public alongside server.js.
  await copyDir(staticDir, path.join(resourcesDir, '.next', 'static'));
  if (await pathExists(publicDir)) await copyDir(publicDir, path.join(resourcesDir, 'public'));

  // Guard: an isolated (default pnpm) layout shows up as hundreds of links into
  // a .pnpm store. Materializing those would balloon the bundle and mask a
  // stale --skip-build, so refuse and ask for a hoisted rebuild.
  const stagedLinks = await findLinks(resourcesDir);
  if (stagedLinks.length > 100) {
    throw new Error(
      `staged tree carries ${stagedLinks.length} links (pnpm isolated layout?) — rerun without --skip-build so the install uses --config.node-linker=hoisted`,
    );
  }

  let nodeVersion: string | null = null;
  let binaryName: string | null = null;
  if (!args.skipNode) {
    nodeVersion = await resolveNode22Latest();
    console.log(`resolved Node 22 LTS: ${nodeVersion}`);
    binaryName = await stageNodeBinary(binariesDir, triple, nodeVersion);
  }

  // Which node binary the smoke boot runs: the staged sidecar when it matches
  // this host (exactly what ships), otherwise the host's own node — a
  // cross-target --target cannot exec the shipped binary, but the tree layout
  // check still applies.
  const nativeTarget = triple === hostTriple();
  const nodeBin =
    binaryName && nativeTarget
      ? path.join(binariesDir, binaryName)
      : process.execPath;
  const smokeLabel =
    binaryName && nativeTarget ? 'bundled sidecar' : 'host node (sidecar not staged for this target)';

  let submoduleSha = 'unknown';
  try {
    submoduleSha = execFileSync('git', ['-C', srcDir, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {
    // non-fatal: meta just records unknown
  }

  const meta = {
    target: triple,
    node: nodeVersion,
    binary: binaryName,
    submoduleSha,
    portStrategy: 'dynamic-loopback',
    // Bumped layout marker: the shipped tree is symlink-free, so the shell has
    // no links to recreate. Changing this string forces every installed app to
    // re-extract exactly once (the shell compares the whole JSON blob).
    layout: 'symlink-free-hoisted',
    stagedAt: new Date().toISOString(),
  };
  await fs.writeFile(path.join(resourcesDir, '.build-meta.json'), JSON.stringify(meta, null, 2));
  console.log('build meta:', JSON.stringify(meta));

  // Ship a tree with ZERO symlinks/junctions. Windows cannot carry links
  // through an installer (bsdtar mangles symlink entries), and restoring
  // links at launch is unreliable, so the links must not exist in the first
  // place. The hoisted (npm-style) node-linker puts every package in a real
  // directory reachable from its ancestors, so links are materialized here
  // rather than transported: host link semantics (macOS symlinks vs Windows
  // junctions vs each installer's quirks) cannot affect the bundle, and the
  // long `.pnpm/<name>@<ver>_<peer-hash>` store segments that push Windows
  // toward the 260-character limit disappear.
  await materializeLinks(resourcesDir);
  await assertNoLinks(resourcesDir);

  // Tauri's resource bundler does not preserve symlinks, and tarballing the
  // tree keeps the installer small and fast. There are no symlinks left to
  // lose, so the archive is a plain directory snapshot.
  const tarballPath = path.join(path.dirname(resourcesDir), 'server.tar.gz');
  await fs.rm(tarballPath, { force: true });
  run('tar', ['-czf', tarballPath, '-C', path.dirname(resourcesDir), 'server']);
  const tarStat = await fs.stat(tarballPath);
  console.log(
    `server tarball: ${(tarStat.size / 1048576).toFixed(1)} MB (unpacked dir staged at ${resourcesDir})`,
  );

  // Build-time gate: unpack the tarball somewhere clean and boot the real
  // server from it, exactly as the desktop shell will on first launch.
  await smokeTestBundledTarball(tarballPath, nodeBin, smokeLabel);
  console.log('prepare-server done.');
  console.log(`NOTE: src-tauri/resources/server/ stays on disk for 'tauri dev'; release bundles ship ${path.basename(tarballPath)}.`);
}

main().catch((err: unknown) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
