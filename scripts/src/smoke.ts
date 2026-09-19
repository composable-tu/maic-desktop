// Build-time gate: boot the packed server from a clean extraction, the way
// the desktop shell does on first launch.

import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import { run } from './shell';

// Boot the packed server from a clean extraction, the way the desktop shell
// does on first launch: unpack the tarball, run `node server.js`, wait for
// /api/health. This is the check that catches a broken bundle before it
// ships: static file probes pass on trees whose physical layout still breaks
// Node's module walk, because Next resolves through the physical layout, not
// the lexical one.
export async function smokeTestBundledTarball(
  tarballPath: string,
  nodeBin: string,
  label: string,
): Promise<void> {
  const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'maic-server-smoke-'));
  const serverDir = path.join(tmp, 'server');
  let child: ReturnType<typeof spawn> | null = null;
  let exited: Promise<void> | null = null;
  let tail = '';
  const keep = (chunk: unknown) => {
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
    child.stdout?.on('data', keep);
    child.stderr?.on('data', keep);
    exited = new Promise<void>((resolve) => {
      if (child!.exitCode !== null || child!.signalCode !== null) resolve();
      else child!.once('exit', () => resolve());
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
async function removeTreeWithRetries(
  dir: string,
  { attempts = 30, delayMs = 1000 } = {},
): Promise<boolean> {
  // EBUSY/EPERM/EACCES are what a locked cwd and an AV scan surface as on
  // Windows; ENOTEMPTY is the partial-delete retry that follows them.
  const retryable = new Set(['EBUSY', 'EPERM', 'EACCES', 'ENOTEMPTY']);
  for (let i = 0; i < attempts; i++) {
    try {
      await fs.rm(dir, { recursive: true, force: true });
      return true;
    } catch (err) {
      const code = (err as NodeJS.ErrnoException | null)?.code;
      if (typeof code !== 'string' || !retryable.has(code)) {
        console.warn(`cleanup of ${dir} failed: ${err instanceof Error ? err.message : err}`);
        return false;
      }
      await sleep(delayMs);
    }
  }
  return false;
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      if (!addr || typeof addr === 'string') {
        reject(new Error('failed to allocate a loopback port'));
        return;
      }
      const { port } = addr;
      srv.close(() => resolve(port));
    });
  });
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));
