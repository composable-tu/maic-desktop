// Shared child-process and filesystem-existence helpers.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs/promises';

export function run(cmd: string, args: string[]): void {
  console.log(`$ ${cmd} ${args.join(' ')}`);
  // On Windows, tools installed via package managers (pnpm, corepack shims)
  // are .CMD wrappers that can only be resolved through the shell.
  const shell = process.platform === 'win32';
  execFileSync(cmd, args, { stdio: 'inherit', shell });
}

export async function pathExists(p: string): Promise<boolean> {
  try {
    await fs.access(p);
    return true;
  } catch {
    return false;
  }
}
