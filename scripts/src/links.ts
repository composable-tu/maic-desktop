// Symlink handling: the staged server tree must ship link-free, so every
// surviving link is materialized into a real copy before packing.

import fs from 'node:fs/promises';
import path from 'node:path';

// Standalone staging copy. `dereference:false` so a link is never silently
// expanded into a partial real dir that no later check can detect; any
// link that survives is materialized by materializeLinks() below, so the
// shipped tree is link-free and identical on every host.
export async function copyDir(src: string, dest: string): Promise<void> {
  await fs.mkdir(dest, { recursive: true });
  await fs.cp(src, dest, { recursive: true, dereference: false });
}

// Copy a file/dir tree, resolving through symlinks (like `cp -rL`).
// `chain` carries the realpath stack being expanded; re-entering one is a
// link cycle, which would otherwise recurse until the path limit.
async function copyResolved(src: string, dest: string, chain: string[] = []): Promise<void> {
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
export async function findLinks(dir: string): Promise<string[]> {
  const out: string[] = [];
  async function walk(cur: string): Promise<void> {
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
export async function materializeLinks(dir: string): Promise<void> {
  let total = 0;
  let dropped = 0;
  for (let pass = 0; ; pass++) {
    const links = await findLinks(dir);
    if (links.length === 0) {
      if (total || dropped) {
        console.log(`materialized ${total} links into real files (${dropped} dangling dropped)`);
      }
      return;
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
export async function assertNoLinks(dir: string): Promise<void> {
  const links = await findLinks(dir);
  if (links.length > 0) {
    throw new Error(
      `staged server tree still has ${links.length} symlinks/junctions (e.g. ${links.slice(0, 5).join(', ')}) — refusing to bundle`,
    );
  }
}
