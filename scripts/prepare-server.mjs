#!/usr/bin/env node
// prepare-server.mjs — build openmaic-src (read-only) and stage the Tauri sidecar payload.
// Never modifies files inside openmaic-src/; it only runs its install/build and copies outputs.
//
// Usage:
//   node scripts/prepare-server.mjs [--target <rust-triple>] [--skip-build] [--skip-node]
// Examples:
//   node scripts/prepare-server.mjs
//   node scripts/prepare-server.mjs --target aarch64-apple-darwin
//   node scripts/prepare-server.mjs --target x86_64-pc-windows-msvc
//
// Output:
//   src-tauri/resources/server/   — standalone server.js + .next/static + public
//   src-tauri/binaries/openmaic-node-<triple>[.exe] — downloaded Node 22 LTS binary
//   src-tauri/resources/server/.build-meta.json — provenance record

import { execFileSync } from 'node:child_process';
import { createWriteStream } from 'node:fs';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const srcDir = path.join(root, 'openmaic-src');
const resourcesDir = path.join(root, 'src-tauri', 'resources', 'server');
const binariesDir = path.join(root, 'src-tauri', 'binaries');

const TRIPLES = {
  'aarch64-apple-darwin': { nodeOs: 'darwin', nodeArch: 'arm64', ext: 'tar.gz', exe: false },
  'x86_64-apple-darwin': { nodeOs: 'darwin', nodeArch: 'x64', ext: 'tar.gz', exe: false },
  'x86_64-pc-windows-msvc': { nodeOs: 'win', nodeArch: 'x64', ext: 'zip', exe: true },
  'aarch64-pc-windows-msvc': { nodeOs: 'win', nodeArch: 'arm64', ext: 'zip', exe: true },
  'x86_64-unknown-linux-gnu': { nodeOs: 'linux', nodeArch: 'x64', ext: 'tar.gz', exe: false },
  'aarch64-unknown-linux-gnu': { nodeOs: 'linux', nodeArch: 'arm64', ext: 'tar.gz', exe: false },
};

function hostTriple() {
  const platform = os.platform();
  const arch = os.arch();
  if (platform === 'darwin') return arch === 'arm64' ? 'aarch64-apple-darwin' : 'x86_64-apple-darwin';
  if (platform === 'win32') return arch === 'arm64' ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
  if (platform === 'linux') return arch === 'arm64' ? 'aarch64-unknown-linux-gnu' : 'x86_64-unknown-linux-gnu';
  throw new Error(`unsupported host platform: ${platform}/${arch}`);
}

function parseArgs(argv) {
  const out = { target: null, skipBuild: false, skipNode: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--target') out.target = argv[++i];
    else if (a === '--skip-build') out.skipBuild = true;
    else if (a === '--skip-node') out.skipNode = true;
    else throw new Error(`unknown arg: ${a}`);
  }
  return out;
}

function run(cmd, args, opts = {}) {
  console.log(`$ ${cmd} ${args.join(' ')}`);
  // On Windows, tools installed via package managers (pnpm, corepack shims)
  // are .CMD wrappers that can only be resolved through the shell.
  const shell = process.platform === 'win32';
  execFileSync(cmd, args, { stdio: 'inherit', shell, ...opts });
}

async function pathExists(p) {
  try {
    await fs.access(p);
    return true;
  } catch {
    return false;
  }
}

async function copyDir(src, dest) {
  await fs.mkdir(dest, { recursive: true });
  // verbatimSymlinks:false would dereference junctions/symlinks into REAL
  // dirs — fatal on Windows, where pnpm uses junctions: the staged tree
  // would carry a partial real copy of e.g. node_modules/next that shadows
  // the store, and Node's module walk would then miss @swc/helpers
  // (MODULE_NOT_FOUND at runtime). dereference:false keeps the link itself
  // (as a junction on Windows) so link detection below sees it.
  await fs.cp(src, dest, { recursive: true, dereference: false });
}

// Standalone staging copy. Next's standalone output mirrors pnpm's symlinked
// layout: top-level entries like node_modules/next are links (often absolute)
// into isolated .pnpm store dirs, and Node relies on realpath-ing through them
// to find isolated deps (@swc/helpers, sharp, …). Expanding links to real
// files breaks that mechanism, so links are preserved — but absolute links
// pointing back at the build machine's source tree would dangle on the user's
// machine. Rewrite them as tree-relative links into the staged copy:
//   <standaloneDir>/…  ->  <resourcesDir>/…
// Collect every symlink/junction under dir as { link, target } pairs, both
// expressed with POSIX separators relative to dir. Written to
// server/.links.json so the desktop shell can restore links on platforms
// where the tarball transport cannot carry them (Windows bsdtar drops them;
// the shell recreates them as junctions). readdir's withFileTypes may report
// junctions as plain directories, so each directory entry is double-checked
// with lstat.
async function collectLinks(dir) {
  const out = [];
  async function walk(cur) {
    const entries = await fs.readdir(cur, { withFileTypes: true });
    for (const e of entries) {
      const p = path.join(cur, e.name);
      let isLink = e.isSymbolicLink();
      if (!isLink && e.isDirectory()) {
        isLink = (await fs.lstat(p)).isSymbolicLink();
      }
      if (isLink) {
        const raw = await fs.readlink(p);
        const abs = path.resolve(path.dirname(p), raw);
        let relTarget;
        if (abs === dir || abs.startsWith(dir + path.sep)) {
          relTarget = path.relative(dir, abs).split(path.sep).join('/');
        } else {
          relTarget = raw.split(path.sep).join('/');
        }
        out.push({
          link: path.relative(dir, p).split(path.sep).join('/'),
          target: relTarget,
        });
      } else if (e.isDirectory()) {
        await walk(p);
      }
    }
  }
  await walk(dir);
  out.sort((a, b) => (a.link < b.link ? -1 : a.link > b.link ? 1 : 0));
  return out;
}

// Copy a file/dir tree, resolving through symlinks (like `cp -rL`).
async function copyResolved(src, dest) {
  const st = await fs.stat(src);
  if (st.isDirectory()) {
    await fs.mkdir(dest, { recursive: true });
    for (const e of await fs.readdir(src)) {
      await copyResolved(path.join(src, e), path.join(dest, e));
    }
  } else {
    await fs.copyFile(src, dest);
  }
}

// Replace every symlink pointing OUTSIDE the staged tree with a real copy
// of its target (or delete it when the target is gone). Next.js standalone
// tracing on Windows leaves links into the workspace root node_modules
// (e.g. D:/a/…/openmaic-src/node_modules/…) that cannot be shipped: the
// manifest only allows tree-relative targets and the Rust side refuses
// absolute/escaping paths. Must run BEFORE collectLinks/stripLinks.
async function materializeExternalLinks(stagedDir) {
  let count = 0;
  async function walk(cur) {
    const entries = await fs.readdir(cur, { withFileTypes: true });
    for (const e of entries) {
      const p = path.join(cur, e.name);
      // Junctions can surface as plain directories in withFileTypes.
      let isLink = e.isSymbolicLink();
      if (!isLink && e.isDirectory()) {
        isLink = (await fs.lstat(p)).isSymbolicLink();
      }
      if (isLink) {
        const raw = await fs.readlink(p);
        const abs = path.resolve(path.dirname(p), raw);
        if (abs === stagedDir || abs.startsWith(stagedDir + path.sep)) {
          continue; // inside the tree: relink/strip handles it later
        }
        let alive = false;
        try {
          await fs.stat(abs);
          alive = true;
        } catch {
          // dangling: drop it, nothing requires it at runtime
        }
        await fs.rm(p);
        if (alive) {
          await copyResolved(abs, p);
          count++;
        }
      } else if (e.isDirectory()) {
        await walk(p);
      }
    }
  }
  await walk(stagedDir);
  console.log(`materialized ${count} external links into the staged tree`);
}

// Delete every symlink (and junction) under dir (files stay). Used before
// packing the tarball: Windows bsdtar cannot extract symlink entries (it
// mangles them into \\?\C:\… paths and aborts with "Invalid argument"). The
// removed links are fully described by .links.json and restored at launch.
// readdir's withFileTypes may report junctions as plain directories on some
// Node/libuv combos, so each directory entry is double-checked with lstat.
async function stripLinks(dir) {
  let count = 0;
  async function walk(cur) {
    const entries = await fs.readdir(cur, { withFileTypes: true });
    for (const e of entries) {
      const p = path.join(cur, e.name);
      if (e.isSymbolicLink()) {
        await fs.rm(p);
        count++;
      } else if (e.isDirectory()) {
        // Junctions can surface as plain directories in withFileTypes;
        // lstat is authoritative (isSymbolicLink covers both links and
        // junctions on Windows).
        const st = await fs.lstat(p);
        if (st.isSymbolicLink()) {
          await fs.rm(p);
          count++;
        } else {
          await walk(p);
        }
      }
    }
  }
  await walk(dir);
  console.log(`stripped ${count} symlinks/junctions before packing`);
}

async function relinkStagedTree(standaloneDir, resourcesDir) {
  async function walk(dir) {
    const entries = await fs.readdir(dir, { withFileTypes: true });
    for (const e of entries) {
      const p = path.join(dir, e.name);
      // Junctions (Windows pnpm layout) can surface as plain directories in
      // withFileTypes — double-check with lstat so every link is rewritten.
      let isLink = e.isSymbolicLink();
      if (!isLink && e.isDirectory()) {
        isLink = (await fs.lstat(p)).isSymbolicLink();
      }
      if (isLink) {
        const raw = await fs.readlink(p);
        const abs = path.resolve(path.dirname(p), raw);
        if (abs === standaloneDir || abs.startsWith(standaloneDir + path.sep)) {
          const rel = path.relative(
            path.dirname(p),
            path.join(resourcesDir, path.relative(standaloneDir, abs)),
          );
          await fs.rm(p);
          // pnpm's layout relies on links resolving INSIDE the tree. On
          // Windows a relative dir symlink needs privileges; a junction
          // (absolute target) is the no-privilege equivalent and is what
          // pnpm itself uses.
          if (process.platform === 'win32') {
            const absTarget = path.resolve(path.dirname(p), rel);
            await fs.rm(p, { force: true, recursive: true });
            await fs.symlink(absTarget, p, 'junction');
          } else {
            await fs.symlink(rel, p);
          }
          // The rewritten target may itself be stale (e.g. pnpm's has-flag
          // ghost entry): drop it if nothing exists there.
          try {
            await fs.stat(p);
          } catch {
            await fs.rm(p, { force: true });
          }
        } else if (!(await pathExists(abs))) {
          await fs.rm(p, { force: true }); // dangling + outside the tree: drop it
        }
        // else: link points outside the tree but target exists — keep as is.
      } else if (e.isDirectory()) {
        await walk(p);
      }
    }
  }
  await walk(resourcesDir);
}

async function resolveNode22Latest() {
  const res = await fetch('https://nodejs.org/dist/index.json');
  if (!res.ok) throw new Error(`failed to fetch nodejs index: ${res.status}`);
  const list = await res.json();
  // index.json is newest-first; first v22.x entry with an lts tag is the current 22 LTS.
  const hit = list.find((e) => typeof e.version === 'string' && e.version.startsWith('v22.') && e.lts);
  if (!hit) throw new Error('could not find a Node 22 LTS release in nodejs index');
  return hit.version; // e.g. "v22.14.0"
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const proto = url.startsWith('https:') ? import('node:https') : import('node:http');
    proto.then(({ default: http }) => {
      const req = http.get(url, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          download(res.headers.location, dest).then(resolve, reject);
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error(`download failed ${res.statusCode}: ${url}`));
          return;
        }
        const out = createWriteStream(dest);
        res.pipe(out);
        out.on('finish', () => out.close(resolve));
        out.on('error', reject);
      });
      req.on('error', reject);
    }, reject);
  });
}

async function stageNodeBinary(triple, version) {
  const spec = TRIPLES[triple];
  if (!spec) throw new Error(`unsupported --target ${triple} (supported: ${Object.keys(TRIPLES).join(', ')})`);
  const base = `node-${version}-${spec.nodeOs}-${spec.nodeArch}`;
  const url = `https://nodejs.org/dist/${version}/${base}.${spec.ext}`;
  const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'maic-node-'));
  const archive = path.join(tmp, `${base}.${spec.ext}`);
  console.log(`downloading ${url}`);
  await download(url, archive);

  // Extract only the node binary.
  const binName = spec.exe ? 'node.exe' : 'node';
  const outName = spec.exe ? `openmaic-node-${triple}.exe` : `openmaic-node-${triple}`;
  const outPath = path.join(binariesDir, outName);
  await fs.mkdir(binariesDir, { recursive: true });

  if (spec.ext === 'zip') {
    run('powershell', ['-NoProfile', '-Command', `Expand-Archive -Path '${archive}' -DestinationPath '${tmp}\\x' -Force`]);
    await fs.copyFile(path.join(tmp, 'x', base, binName), outPath);
  } else {
    run('tar', ['-xzf', archive, '-C', tmp]);
    await fs.copyFile(path.join(tmp, base, 'bin', 'node'), outPath);
    await fs.chmod(outPath, 0o755);
    // macOS attaches a provenance/XProtect marker to freshly downloaded files
    // that SIGKILLs them on exec. A copy round-trip sheds the enforcement
    // while keeping the official Node signature intact.
    const shuffled = `${outPath}.stage`;
    await fs.copyFile(outPath, shuffled);
    await fs.rename(shuffled, outPath);
    await fs.chmod(outPath, 0o755);
    // NOTE: do NOT ad-hoc re-sign here. An ad-hoc signature drops the
    // Team ID, and AMFI kills GUI-app-spawned ad-hoc binaries on launch
    // while letting the same binary run from a terminal. Keep the official
    // Node.js Developer ID signature; Dock handling is done at runtime.

  }
  await fs.rm(tmp, { recursive: true, force: true });
  // Sanity: a 0-byte (or missing) placeholder must never be bundled as the
  // sidecar — it would install fine and then die silently at launch.
  const binStat = await fs.stat(outPath);
  const MIN_NODE_BYTES = 10 * 1024 * 1024;
  if (binStat.size < MIN_NODE_BYTES) {
    throw new Error(
      `staged sidecar too small (${binStat.size} bytes): ${outPath} — refusing to bundle`,
    );
  }
  console.log(`staged node binary: ${outPath}`);
  return outName;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const triple = args.target || hostTriple();
  console.log(`prepare-server: target=${triple}`);

  if (!(await pathExists(path.join(srcDir, 'package.json')))) {
    throw new Error(`submodule not checked out: ${srcDir} (run: git submodule update --init --recursive)`);
  }

  if (!args.skipBuild) {
    run('pnpm', ['--dir', srcDir, 'install', '--frozen-lockfile']);
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
  await relinkStagedTree(standaloneDir, resourcesDir);
  // Dockerfile parity: standalone output expects .next/static and public alongside server.js.
  await copyDir(staticDir, path.join(resourcesDir, '.next', 'static'));
  if (await pathExists(publicDir)) await copyDir(publicDir, path.join(resourcesDir, 'public'));

  let nodeVersion = null;
  let binaryName = null;
  if (!args.skipNode) {
    nodeVersion = await resolveNode22Latest();
    console.log(`resolved Node 22 LTS: ${nodeVersion}`);
    binaryName = await stageNodeBinary(triple, nodeVersion);
  }

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
    stagedAt: new Date().toISOString(),
  };
  await fs.writeFile(path.join(resourcesDir, '.build-meta.json'), JSON.stringify(meta, null, 2));
  console.log('build meta:', JSON.stringify(meta));

  // Fold workspace-external links (Windows tracing leaves absolute links
  // into the source node_modules) into real files BEFORE the manifest is
  // collected — the manifest only allows tree-relative targets.
  await materializeExternalLinks(resourcesDir);

  // Symlink manifest for restoring links after extraction. The shell
  // recreates every entry at first launch (junctions on Windows, symlinks
  // elsewhere) because the tarball below ships zero symlinks.
  const links = await collectLinks(resourcesDir);
  await fs.writeFile(
    path.join(resourcesDir, '.links.json'),
    JSON.stringify(links, null, 2),
  );
  console.log(`link manifest: ${links.length} links`);

  // Remove every symlink from the staged tree BEFORE packing. Windows bsdtar
  // turns symlink entries into \\?\C:\…-prefixed paths at extract time,
  // which fails with "Invalid argument" and aborts the whole extraction.
  // The tree is fully described by .links.json, so nothing is lost.
  await stripLinks(resourcesDir);

  // Tauri's resource bundler does not preserve symlinks (it materializes them
  // or drops them), which breaks pnpm's isolated-deps layout — and Windows
  // bsdtar mangles symlink entries into \\?\C:\… paths and aborts extraction.
  // So the tarball ships zero symlinks (stripped above); the shell restores
  // them from .links.json at first launch. The tarball also shrinks the
  // installer substantially.
  const tarballPath = path.join(path.dirname(resourcesDir), 'server.tar.gz');
  await fs.rm(tarballPath, { force: true });
  run('tar', ['-czf', tarballPath, '-C', path.dirname(resourcesDir), 'server']);
  const tarStat = await fs.stat(tarballPath);
  const dirStat = await fs.stat(resourcesDir);
  console.log(
    `server tarball: ${(tarStat.size / 1048576).toFixed(1)} MB (unpacked dir staged at ${resourcesDir})`,
  );
  void dirStat;
  console.log('prepare-server done.');
  console.log(`NOTE: src-tauri/resources/server/ stays on disk for 'tauri dev'; release bundles ship ${path.basename(tarballPath)}.`);
}

main().catch((err) => {
  console.error(err?.message || err);
  process.exit(1);
});
