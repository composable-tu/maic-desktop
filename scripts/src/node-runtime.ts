// The embedded Node.js 22 LTS runtime: version resolution, download, and
// staging as the Tauri sidecar binary.

import { createWriteStream } from 'node:fs';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import { TRIPLES } from './triples';
import { run } from './shell';

//nodejs.org/dist/index.json entries; only the fields used here.
interface NodeRelease {
  version: string;
  lts: string | false;
}

export async function resolveNode22Latest(): Promise<string> {
  const res = await fetch('https://nodejs.org/dist/index.json');
  if (!res.ok) throw new Error(`failed to fetch nodejs index: ${res.status}`);
  const list = (await res.json()) as NodeRelease[];
  // index.json is newest-first; first v22.x entry with an lts tag is the current 22 LTS.
  const hit = list.find((e) => e.version.startsWith('v22.') && e.lts);
  if (!hit) throw new Error('could not find a Node 22 LTS release in nodejs index');
  return hit.version; // e.g. "v22.14.0"
}

async function download(url: string, dest: string): Promise<void> {
  const res = await fetch(url);
  if (!res.ok || !res.body) throw new Error(`download failed ${res.status}: ${url}`);
  await pipeline(Readable.fromWeb(res.body), createWriteStream(dest));
}

export async function stageNodeBinary(
  binariesDir: string,
  triple: string,
  version: string,
): Promise<string> {
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
