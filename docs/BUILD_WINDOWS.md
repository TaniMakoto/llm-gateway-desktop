# Windows 本地构建

## 环境

安装：

1. Node.js（版本见仓库根目录 `.node-version`）
2. pnpm 10
3. Rust stable，满足 `src-tauri/Cargo.toml` 中的 `rust-version`
4. Visual Studio 2022 Build Tools，并选择“使用 C++ 的桌面开发”
5. Microsoft Edge WebView2 Runtime

## 构建

```powershell
corepack enable
corepack prepare pnpm@10.12.3 --activate
pnpm install --frozen-lockfile
pnpm typecheck
cargo test --locked --manifest-path src-tauri/Cargo.toml gateway --lib
pnpm tauri build
```

输出通常位于：

```text
src-tauri\target\release\bundle\
```

## 开发模式

```powershell
pnpm install --frozen-lockfile
pnpm tauri dev
```

## 不安装 Rust 的构建方式

使用 `.github/workflows/build-release.yml`：

1. 将仓库推送至 GitHub。
2. 打开仓库的 **Actions** 页面。
3. 选择 **Build and Release**。
4. 点击 **Run workflow**。
5. 构建完成后下载四个平台的 artifacts。

命令行方式见 [GITHUB_ACTIONS_BUILD.md](GITHUB_ACTIONS_BUILD.md)。
