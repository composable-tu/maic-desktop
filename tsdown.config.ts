import { defineConfig } from 'tsdown';

export default defineConfig({
  entry: ['scripts/src/prepare-server.ts'],
  outDir: 'scripts/dist',
  format: 'esm',
  platform: 'node',
  target: 'node22',
  // The script ships inside the wrapper's build toolchain: keep it readable
  // and debuggable, no declarations, no hashed file names.
  minify: false,
  dts: false,
  hash: false,
  fixedExtension: true,
  // The entry has no shebang in source (it never runs unbundled); inject it
  // deterministically here.
  banner: '#!/usr/bin/env node',
});
