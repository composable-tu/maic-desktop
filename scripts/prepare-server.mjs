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

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream } from 'node:fs';
import { createServer } from 'node:http';
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

// Standalone staging copy. `dereference:false` so a link is never silently
// expanded into a partial real dir (fatal under pnpm's isolated layout); any
// link that survives is materialized by materializeLinks() below, so the
// shipped tree is link-free and identical on every host.
async function copyDir(src, dest) {
  await fs.mkdir(dest, { recursive: true });
  await fs.cp(src, dest, { recursive: true, dereference: false });
}

// Copy a file/dir tree, resolving through symlinks (like `cp -rL`).
// `chain` carries the realpath stack being expanded; re-entering one is a
// link cycle, which would otherwise recurse until the path limit.
async function copyResolved(src, dest, chain = []) {
  const real = await fs.realpath(src).catch(() => path.resolve(src));
  if (chain.includes(real)) {
    throw new Error(`symlink cycle while materializing ${src} (-> ${real})`);
  }
  const st = await fs.stat(src);
  if (st.isDirectory()) {
    await fs.mkdir(dest, { recursive: true });
    for (const e of await fs.readdir(src)) {
      await copyResolved(path.join(src, e), path.join(dest, e), [...chain, real]);
    }
  } else {
    await fs.copyFile(src, dest);
  }
}

// Every symlink/junction under dir, as tree-relative POSIX paths. Junctions
// can surface as plain directories in readdir's withFileTypes, so each
// directory entry is double-checked with lstat (authoritative on Windows too).
async function findLinks(dir) {
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
        out.push(path.relative(dir, p).split(path.sep).join('/'));
      } else if (e.isDirectory()) {
        await walk(p);
      }
    }
  }
  await walk(dir);
  return out;
}

// Replace every remaining symlink/junction with a real copy of its target
// (dangling links are dropped), until the tree is link-free. Materializing an
// outer link can expose inner ones, so this re-scans until findLinks is empty.
//
// This is what makes the bundle platform-agnostic: after this pass the tarball
// carries no link entries, Windows bsdtar has nothing to mangle, the shell has
// nothing to recreate (no `mklink`, no privilege/AV/long-path failure modes),
// and Node resolves from the same physical layout it was tested with at build
// time. Safe because the staged tree is a hoisted (npm-style) node_modules: a
// package's dependencies are reachable from its ancestors even when the
// package sits at the link path rather than inside a .pnpm store dir.
async function materializeLinks(dir) {
  let total = 0;
  let dropped = 0;
  for (let pass = 0; ; pass++) {
    const links = await findLinks(dir);
    if (links.length === 0) {
      if (total || dropped) {
        console.log(`materialized ${total} links into real files (${dropped} dangling dropped)`);
      }
      return { total, dropped };
    }
    if (pass > 9) {
      throw new Error(`links keep appearing after ${pass} passes — ${links.length} left, e.g. ${links[0]}`);
    }
    for (const rel of links) {
      const p = path.join(dir, ...rel.split('/'));
      const raw = await fs.readlink(p);
      const abs = path.resolve(path.dirname(p), raw);
      await fs.rm(p);
      let alive = true;
      try {
        await fs.stat(abs);
      } catch {
        alive = false;
      }
      if (!alive) {
        dropped++;
        continue;
      }
      await copyResolved(abs, p);
      total++;
    }
  }
}

// Hard gate: a single surviving link means the shipped tree differs per
// platform (and Windows is the one that breaks), so refuse to bundle it.
async function assertNoLinks(dir) {
  const links = await findLinks(dir);
  if (links.length > 0) {
    throw new Error(
      `staged server tree still has ${links.length} symlinks/junctions (e.g. ${links.slice(0, 5).join(', ')}) — refusing to bundle`,
    );
  }
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

// Boot the packed server from a clean extraction, the way the desktop shell
// does on first launch: unpack the tarball, run `node server.js`, wait for
// /api/health. This is the only check that catches a broken bundle before it
// ships — static "can the file be stat'd" probes passed on Windows right up to
// Node's own module walk failing, because Next resolves through the physical
// layout, not the lexical one.
async function smokeTestBundledTarball(tarballPath, nodeBin, label) {
  const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'maic-server-smoke-'));
  const serverDir = path.join(tmp, 'server');
  let child = null;
  let exited = null;
  let tail = '';
  const keep = (chunk) => {
    tail = (tail + String(chunk)).slice(-8000);
  };
  try {
    run('tar', ['-xzf', tarballPath, '-C', tmp]);
    const port = await freePort();
    const url = `http://127.0.0.1:${port}/api/health`;
    console.log(`smoke boot: ${nodeBin} → ${url} (${label})`);
    child = spawn(nodeBin, [path.join(serverDir, 'server.js')], {
      cwd: serverDir,
      env: {
        ...process.env,
        PORT: String(port),
        HOSTNAME: '127.0.0.1',
        NODE_ENV: 'production',
        NEXT_TELEMETRY_DISABLED: '1',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    child.stdout.on('data', keep);
    child.stderr.on('data', keep);
    exited = new Promise((resolve) => {
      if (child.exitCode !== null || child.signalCode !== null) resolve();
      else child.once('exit', resolve);
    });

    const deadline = Date.now() + 120_000;
    for (;;) {
      if (child.exitCode !== null || child.signalCode !== null) {
        const how = child.exitCode !== null ? `code ${child.exitCode}` : `signal ${child.signalCode}`;
        throw new Error(
          `smoke boot failed: server exited with ${how} before serving ${url}\n` +
            `--- server output ---\n${tail}`,
        );
      }
      try {
        if ((await fetch(url)).ok) {
          console.log('smoke boot: server is healthy, bundle verified');
          return;
        }
      } catch {
        // not listening yet
      }
      if (Date.now() > deadline) {
        throw new Error(`smoke boot failed: ${url} never answered\n--- server output ---\n${tail}`);
      }
      await sleep(250);
    }
  } finally {
    if (child && child.exitCode === null && child.signalCode === null) child.kill();
    // The child's cwd lives inside the temp tree, so nothing under it can be
    // removed until the process is actually reaped.
    if (exited) await Promise.race([exited, sleep(10_000)]);
    // Then Windows can still hold the directory a while longer (a killed
    // node.exe releases its cwd handle late, and Defender scans a freshly
    // unpacked 189 MB tree). The verdict is already recorded by this point, so
    // a stubborn temp dir is reported and skipped rather than failing a build
    // whose bundle just booted healthy.
    if (!(await removeTreeWithRetries(tmp))) {
      console.warn(`smoke boot: could not clean up ${tmp} (still locked); leaving it to the OS temp cleaner`);
    }
  }
}

// Retry an unlink of a tree a killed process may still hold. Never throws: the
// caller decides whether a leftover temp dir matters, and a cleanup error must
// not mask the smoke test's own failure.
async function removeTreeWithRetries(dir, { attempts = 30, delayMs = 1000 } = {}) {
  // EBUSY/EPERM/EACCES are what a locked cwd and an AV scan surface as on
  // Windows; ENOTEMPTY is the partial-delete retry that follows them.
  const retryable = new Set(['EBUSY', 'EPERM', 'EACCES', 'ENOTEMPTY']);
  for (let i = 0; i < attempts; i++) {
    try {
      await fs.rm(dir, { recursive: true, force: true });
      return true;
    } catch (err) {
      if (!retryable.has(err?.code)) {
        console.warn(`cleanup of ${dir} failed: ${err?.message || err}`);
        return false;
      }
      await sleep(delayMs);
    }
  }
  return false;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
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

  let nodeVersion = null;
  let binaryName = null;
  if (!args.skipNode) {
    nodeVersion = await resolveNode22Latest();
    console.log(`resolved Node 22 LTS: ${nodeVersion}`);
    binaryName = await stageNodeBinary(triple, nodeVersion);
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

  // Ship a tree with ZERO symlinks/junctions. Under pnpm's default isolated
  // layout that was impossible (Node only finds `next`'s deps inside the
  // .pnpm store dir, so the links had to survive the trip through the
  // installer — and Windows cannot carry them: bsdtar mangles symlink entries,
  // and junctions restored at launch proved unreliable). Installing the
  // submodule with a hoisted (npm-style) node-linker instead puts every
  // package in a real directory reachable from its ancestors, so links are
  // materialized here rather than transported. Host link semantics (macOS
  // symlinks vs Windows junctions vs each installer's quirks) no longer affect
  // the bundle, and long-path pressure on Windows drops away with the
  // `.pnpm/<name>@<ver>_<peer-hash>` store segments.
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
  // server from it, exactly as the desktop shell will on first launch. This is
  // the only check that reproduces Windows' failure mode — static probes passed
  // there while Node's own module walk did not.
  await smokeTestBundledTarball(tarballPath, nodeBin, smokeLabel);
  console.log('prepare-server done.');
  console.log(`NOTE: src-tauri/resources/server/ stays on disk for 'tauri dev'; release bundles ship ${path.basename(tarballPath)}.`);
}

main().catch((err) => {
  console.error(err?.message || err);
  process.exit(1);
});
