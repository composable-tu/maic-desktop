# MAIC Desktop

Tauri desktop wrapper for [OpenMAIC](https://github.com/THU-MAIC/OpenMAIC), consumed as a
read-only git submodule in `openmaic-src/`. **Nothing inside `openmaic-src/` is ever modified
by this wrapper** — all desktop code lives in `src-tauri/`, `scripts/`, and `.github/`.

## How it works

OpenMAIC is a full-stack Next.js SSR app (API routes, middleware, pg/S3/sharp), so it cannot
be statically exported. Instead the desktop app ships the Next.js **standalone server**
(`.next/standalone/server.js`) plus an **embedded Node.js 22 LTS binary** as a Tauri sidecar:

- **Dev** (`pnpm dev`): Tauri window points at the submodule's `next dev` on `localhost:3000`.
- **Production**: on launch, the Rust shell ensures the server runtime is extracted
  (see below), picks a loopback port — **31846 when free**, otherwise the
  previously recorded sticky port, otherwise a fresh random one (recorded in
  `server-port.json` in the
  app-data dir and reused while free — this keeps the origin stable so IndexedDB,
  localStorage and the Cache API persist across launches; a second instance gets a
  fresh port), spawns the Node sidecar from outside the bundle (see Dock note) with
  `PORT`/`HOSTNAME` set, waits for `/api/health` (~60 s timeout on first launch while
  the runtime extracts, ~20 s after), then opens the main window against it.
  No system Node.js required.
- Multiple instances each get their own port; exiting the app terminates its sidecar.

## Prerequisites

- Node.js ≥ 22.19, pnpm 10.28 (`corepack enable` recommended)
- Rust stable toolchain
- Linux only: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`
- First checkout: `git submodule update --init --recursive`

## Commands

| Command                          | What it does                                              |
| -------------------------------- | --------------------------------------------------------- |
| `pnpm dev`                       | Submodule `next dev` + Tauri window (port 3000)           |
| `node scripts/prepare-server.mjs`| Build submodule, stage standalone server + Node sidecar   |
| `pnpm build` / `pnpm tauri build`| `prepare-server` (via `beforeBuildCommand`) + Tauri bundle |

`prepare-server.mjs` options:

- `--target <rust-triple>`: which Node binary to download (default: host).
  Supported: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
  `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
- `--skip-build`: reuse the existing `.next/` output.
- `--skip-node`: stage the server without (re)downloading Node.

Staged payload (all gitignored, generated at build time):

- `src-tauri/resources/server/` — unpacked standalone tree (kept on disk for `tauri dev`)
- `src-tauri/resources/server.tar.gz` (~200 MB) — the same tree as a tarball; this is
  what ships inside the installer. A tarball is required because app bundlers do not
  preserve symlinks, and pnpm's isolated-deps layout depends on them.
- `src-tauri/binaries/openmaic-node-<triple>[.exe]` — official Node 22 LTS binary,
  exact version pinned in `.build-meta.json` (follows latest 22-LTS at build time).

At runtime, the app extracts the tarball to the OS app-data dir on first launch
(e.g. `~/Library/Application Support/com.maic.desktop/server` on macOS), guarded by a
`.build-meta.json` marker so updates re-extract exactly once. Extraction needs the
system `tar` (preinstalled on macOS, mainstream Linux, and Windows 10+), and
extraction failures now surface tar's own stderr instead of a bare exit code.

Windows note: the stock tar backend (bsdtar) mangles symlink entries into
`\\?\C:\…` paths and aborts extraction with "Invalid argument" (previously
misdiagnosed as silently dropped links). `prepare-server` therefore strips all
294 symlinks before packing — the tarball ships zero links — and writes a
`server/.links.json` manifest; on first launch the shell restores directory
links as **real directory copies** on Windows (junctions proved unreliable
under Node's module walk: the nested `@swc/helpers` junction was unusable to
Node while every `fs::canonicalize`-based check passed, surfacing as
`Cannot find module '@swc/helpers/…'`) and file links as plain copies. Other
platforms restore the same manifest as symlinks, so behavior is identical
everywhere.

macOS Dock note: the Node sidecar binary is copied out of the `.app` bundle into
`app-data/bin/` before launch, and is re-signed ad-hoc at stage time. Without this,
LaunchServices enrolls any executable inside `Contents/MacOS/` as a Foreground app
under our bundle id — and since the server never opens a window, its Dock tile
bounces forever. Outside the bundle it registers as BackgroundOnly and stays out
of the Dock (verified with `lsappinfo list`).

`tauri dev` note: dev builds read `src-tauri/resources/server/` in place. After a fresh
`prepare-server` run, sync it into the dev profile once with
`cp -R src-tauri/resources/server src-tauri/target/debug/resources/`.

## CI
 
- `desktop-check.yml` (PR / push to main touching wrapper files): submodule build +
  `cargo check` + `tauri build --no-bundle` smoke on Ubuntu.
- `desktop-build.yml` (push to main, `desktop-v*` tags, manual dispatch): full bundles on
  4 runners — mac-arm64, mac-x64, win-x64, win-arm64. Uploads `.dmg` (mac) /
  nsis+msi (win) to **Actions Artifacts, 90-day retention**. No GitHub Releases are created;
  `desktop-v*` tags are build triggers only.
- macOS builds are **unsigned** for now: first launch needs right-click → Open.
  Windows/macOS signing is a future opt-in (needs `APPLE_*` / `TAURI_SIGNING_*` secrets).

## Versioning

- Wrapper version (`package.json`, `tauri.conf.json`, `Cargo.toml`) starts at `0.1.0`
  and is independent of the submodule's version (currently OpenMAIC `1.0.3`).
- Traceability comes from `src-tauri/resources/server/.build-meta.json`
  (submodule SHA + embedded Node version + target).

## Deliberately out of scope (v1)

Code signing/notarization, auto-updater, tray, autostart, deep links.
