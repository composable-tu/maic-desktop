# MAIC Desktop

[English](README.desktop.md) | 简体中文

Tauri 桌面封装，包装 [OpenMAIC](https://github.com/THU-MAIC/OpenMAIC)。OpenMAIC 以只读
git 子模块的形式放在 `openmaic-src/`，**本封装绝不修改 `openmaic-src/` 内的任何内容** ——
全部桌面代码都在 `src-tauri/`、`scripts/` 和 `.github/` 中。

## 工作原理

OpenMAIC 是一个全栈 Next.js SSR 应用（API 路由、middleware、pg/S3/sharp），无法静态导出。
因此桌面应用打包的是 Next.js **standalone server**（`.next/standalone/server.js`），外加一个
作为 Tauri sidecar 的**内嵌 Node.js 22 LTS 二进制**：

- **开发**（`pnpm dev`）：Tauri 窗口直接指向子模块在 `localhost:3000` 上运行的 `next dev`。
- **生产**：启动时，Rust 壳先确保服务运行时已解压（见下文），然后选择一个本地回环端口
  —— **31846 空闲时优先**，否则复用上次记录的粘性端口，否则随机取一个新端口（记录在
  应用数据目录的 `server-port.json` 中，空闲即复用 —— 这样 origin 保持稳定，IndexedDB、
  localStorage 和 Cache API 得以跨启动保留；第二个实例会拿到新端口）；接着从应用包外
  启动 Node sidecar（见 macOS Dock 一节），设置 `PORT`/`HOSTNAME`，等待 `/api/health`
  （健康检查超时 60 秒），最后让主窗口指向它。无需系统安装 Node.js。
- 多实例各自持有端口；应用退出时会终止自己的 sidecar。

## 环境要求

- Node.js ≥ 22.19，pnpm 10.28（建议 `corepack enable`）
- Rust stable 工具链
- 仅 Linux：`libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`
- 首次检出：`git submodule update --init --recursive`

## 常用命令

| 命令                             | 作用                                                        |
| -------------------------------- | ----------------------------------------------------------- |
| `pnpm dev`                       | 子模块 `next dev` + Tauri 窗口（端口 3000）                 |
| `pnpm prepare:server`            | 用 tsdown 打包构建脚本、构建子模块、staging 服务端 + Node sidecar |
| `pnpm typecheck`                 | 类型检查构建脚本（`scripts/src`，strict TS）                |
| `pnpm build` / `pnpm tauri build`| `prepare-server`（经 `beforeBuildCommand`）+ Tauri 打包     |

构建脚本源码在 `scripts/src/`（strict TypeScript），由 tsdown 打包为已被 gitignore 的
`scripts/dist/prepare-server.mjs` —— 上表中的命令是唯一入口。

`prepare-server` 选项：

- `--target <rust-triple>`：下载哪个 Node 二进制（默认当前主机）。支持：
  `aarch64-apple-darwin`、`x86_64-apple-darwin`、`x86_64-pc-windows-msvc`、
  `aarch64-pc-windows-msvc`、`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`。
- `--skip-build`：复用已有的 `.next/` 产物。要求子模块当前的 `node_modules` 已经是
  hoisted 布局（见下文说明），否则 staging 会拒绝打包 isolated 布局的目录树。
- `--skip-node`：staging 服务端时跳过（重新）下载 Node。

子模块安装时使用 **`--config.node-linker=hoisted`**（npm 式平铺 `node_modules`，没有
`.pnpm` 符号链接农场），纯粹是为了服务 staging 这一步 —— 封装本身从不改动
`openmaic-src/` 内部。这正是产物可移植的关键：交付的目录树**零符号链接/junction**，
没有任何东西需要穿过安装器还能存活。

Staging 产物（全部已被 gitignore，构建时生成）：

- `src-tauri/resources/server/` — 解包后的 standalone 目录树（留在磁盘上供 `tauri dev` 使用）
- `src-tauri/resources/server.tar.gz`（约 220 MB）— 同一棵树的 tar 包；安装器里装的就是它。
  目录树是纯目录快照，归档里不含链接条目；保留 tarball 是因为打包约 3 万个小文件远比
  打包一个文件慢。
- `src-tauri/binaries/openmaic-node-<triple>[.exe]` — 官方 Node 22 LTS 二进制，精确版本
  记录在 `.build-meta.json`（构建时跟随最新 22-LTS）。

运行时，应用在首次启动把 tarball 解压到系统应用数据目录（如 macOS 上的
`~/Library/Application Support/com.maic.desktop/server`），由 `.build-meta.json` 标记守卫，
更新时恰好重新解压一次。解压依赖系统 `tar`（macOS、主流 Linux 和 Windows 10+ 均预装），
解压失败会直接透出 tar 自己的 stderr。

Windows 说明：staged 树是 hoisted 且零链接的，必须保持这一状态。pnpm 默认的 isolated
布局携带数百个符号链接/junction：安装器无法搬运它们（bsdtar 会把符号链接条目改写成
`\\?\C:\…` 路径），启动时恢复链接也不可靠 —— 基于 junction 的恢复会破坏 Node 的模块
解析，而所有基于 `fs::canonicalize` 的检查却依然通过。交付 hoisted、零链接的树消除了
整类故障 —— 还顺带甩掉了把 Windows 推向 260 字符路径上限的
`.pnpm/<name>@<ver>_<peer-hash>` 长路径段。不要把符号链接重新引入 staged 树：
`prepare-server` 在打包前断言零链接，壳里也没有任何链接恢复代码。

macOS Dock 说明：Node sidecar 二进制会在启动前从 `.app` 包内复制到 `app-data/bin/`。
不做这一步，LaunchServices 会把 `Contents/MacOS/` 内的任何可执行文件注册为挂在我们
bundle id 下的前台应用 —— 而这个服务永远不开窗口，它的 Dock 图标就会永远弹跳。放到
包外之后，它会注册为 BackgroundOnly，不再出现在 Dock 里。二进制全程保留 Node.js
官方签名（`prepare-server` 从不重签名 —— 见脚本内说明）。

`tauri dev` 说明：开发构建直接就地读取 `src-tauri/resources/server/`。刚跑完一次
`prepare-server` 后，用
`cp -R src-tauri/resources/server src-tauri/target/debug/resources/`
把它同步进 dev profile 一次即可。

## CI
 
- `desktop-check.yml`（触及 wrapper 文件的 PR / main 推送）：wrapper 脚本类型检查 +
  子模块构建 + `cargo check` + `tauri build --no-bundle` 冒烟（Ubuntu）。
- `desktop-build.yml`（`[build]` 前缀的 main 推送、手动触发）：4 个 runner 全量打包 ——
  mac-arm64、mac-x64、win-x64、win-arm64。每个 job 都由 `prepare-server` 的冒烟启动把关：
  解压打包完成的 `server.tar.gz`，并在该 job 自己的操作系统上等待 `/api/health` 响应。
  `.dmg`（mac）/ nsis+msi（win）上传至 **Actions Artifacts（保留 90 天）**。不创建
  GitHub Releases。
- macOS 构建暂为**未签名**：首次启动需右键 → 打开。Windows/macOS 签名是后续可选事项
  （需要 `APPLE_*` / `TAURI_SIGNING_*` secrets）。

## 版本

- 封装版本（`package.json`、`tauri.conf.json`、`Cargo.toml`）从 `0.1.0` 起，独立于子模块
  版本（当前为 OpenMAIC `1.0.3`）。
- 溯源信息在 `src-tauri/resources/server/.build-meta.json`（OpenMAIC 版本 + 子模块 SHA +
  内嵌 Node 版本 + 目标三元组）。

## v1 明确不做

代码签名/公证、自动更新、托盘、开机自启、深链。
