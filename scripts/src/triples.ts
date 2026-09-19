// Target triples → Node.js distribution coordinates, and the host's own triple.

import os from 'node:os';

export interface TripleSpec {
  nodeOs: string;
  nodeArch: string;
  ext: 'tar.gz' | 'zip';
  exe: boolean;
}

export const TRIPLES: Record<string, TripleSpec> = {
  'aarch64-apple-darwin': { nodeOs: 'darwin', nodeArch: 'arm64', ext: 'tar.gz', exe: false },
  'x86_64-apple-darwin': { nodeOs: 'darwin', nodeArch: 'x64', ext: 'tar.gz', exe: false },
  'x86_64-pc-windows-msvc': { nodeOs: 'win', nodeArch: 'x64', ext: 'zip', exe: true },
  'aarch64-pc-windows-msvc': { nodeOs: 'win', nodeArch: 'arm64', ext: 'zip', exe: true },
  'x86_64-unknown-linux-gnu': { nodeOs: 'linux', nodeArch: 'x64', ext: 'tar.gz', exe: false },
  'aarch64-unknown-linux-gnu': { nodeOs: 'linux', nodeArch: 'arm64', ext: 'tar.gz', exe: false },
};

export function hostTriple(): string {
  const platform = os.platform();
  const arch = os.arch();
  if (platform === 'darwin') return arch === 'arm64' ? 'aarch64-apple-darwin' : 'x86_64-apple-darwin';
  if (platform === 'win32') return arch === 'arm64' ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
  if (platform === 'linux') return arch === 'arm64' ? 'aarch64-unknown-linux-gnu' : 'x86_64-unknown-linux-gnu';
  throw new Error(`unsupported host platform: ${platform}/${arch}`);
}
