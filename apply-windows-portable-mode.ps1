param(
    [string]$ProjectRoot = (Get-Location).Path
)

$ErrorActionPreference = "Stop"
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$root = (Resolve-Path $ProjectRoot).Path

function Read-Utf8([string]$RelativePath) {
    $path = Join-Path $root $RelativePath
    if (-not (Test-Path $path)) { throw "Missing required file: $RelativePath" }
    return [System.IO.File]::ReadAllText($path)
}

function Write-Utf8([string]$RelativePath, [string]$Content) {
    $path = Join-Path $root $RelativePath
    $parent = Split-Path $path -Parent
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    [System.IO.File]::WriteAllText($path, $Content, $utf8NoBom)
}

function Replace-Once([string]$RelativePath, [string]$Old, [string]$New, [string]$AlreadyPresent) {
    $content = Read-Utf8 $RelativePath
    if ($AlreadyPresent -and $content.Contains($AlreadyPresent)) {
        Write-Host "Already updated: $RelativePath"
        return
    }
    if (-not $content.Contains($Old)) {
        throw "Expected source block was not found in $RelativePath. The repository may have changed; do not commit partial edits."
    }
    Write-Utf8 $RelativePath ($content.Replace($Old, $New))
    Write-Host "Updated: $RelativePath"
}

$required = @(
    "src-tauri/src/lib.rs",
    "src-tauri/src/config.rs",
    "src-tauri/src/settings.rs",
    "src-tauri/src/app_store.rs",
    ".github/workflows/build-release.yml",
    "src-tauri/tauri.conf.json"
)
foreach ($item in $required) {
    if (-not (Test-Path (Join-Path $root $item))) { throw "Run this script from the repository root. Missing: $item" }
}

$backup = Join-Path (Split-Path $root -Parent) ((Split-Path $root -Leaf) + "-portable-backup-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
New-Item -ItemType Directory -Force -Path $backup | Out-Null
foreach ($item in $required + @("README.md")) {
    $source = Join-Path $root $item
    if (Test-Path $source) {
        $dest = Join-Path $backup $item
        New-Item -ItemType Directory -Force -Path (Split-Path $dest -Parent) | Out-Null
        Copy-Item $source $dest -Force
    }
}
Write-Host "Backup created: $backup"

$portableRs = @'
//! Portable-mode detection and path preparation.
//!
//! When a `portable.flag` file exists next to the executable, application-owned
//! persistent data is stored in the sibling `data` directory. This keeps the
//! portable ZIP self-contained while leaving installed builds unchanged.

use std::path::{Path, PathBuf};

const PORTABLE_FLAG_FILE: &str = "portable.flag";
const PORTABLE_DATA_DIR: &str = "data";

/// Pure helper used by tests and by runtime detection.
fn portable_root_from_executable(executable: &Path) -> Option<PathBuf> {
    let root = executable.parent()?;
    root.join(PORTABLE_FLAG_FILE)
        .is_file()
        .then(|| root.to_path_buf())
}

/// Returns the portable package root when portable mode is enabled.
///
/// `LLM_GATEWAY_PORTABLE_ROOT` is intentionally supported for automated tests
/// and advanced launchers. Normal users enable portable mode with
/// `portable.flag` next to the executable.
pub fn root_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("LLM_GATEWAY_PORTABLE_ROOT") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }

    let executable = std::env::current_exe().ok()?;
    portable_root_from_executable(&executable)
}

/// Returns `<portable root>/data` when portable mode is enabled.
pub fn data_dir() -> Option<PathBuf> {
    root_dir().map(|root| root.join(PORTABLE_DATA_DIR))
}

pub fn is_portable() -> bool {
    root_dir().is_some()
}

/// Creates portable directories and redirects Windows WebView2 browser data.
///
/// This must run before the Tauri builder creates its webview.
pub fn prepare_runtime() -> std::io::Result<Option<PathBuf>> {
    let Some(data_dir) = data_dir() else {
        return Ok(None);
    };

    std::fs::create_dir_all(&data_dir)?;

    #[cfg(target_os = "windows")]
    {
        let webview_dir = data_dir.join("webview2");
        std::fs::create_dir_all(&webview_dir)?;
        // WebView2 reads this process-scoped override while creating the
        // environment. Keeping the UDF here avoids browser cache/profile data
        // being written to the user's LocalAppData directory.
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &webview_dir);
    }

    Ok(Some(data_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_portable_detects_flag_next_to_executable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exe = temp.path().join("LLM Gateway Desktop.exe");
        std::fs::write(&exe, b"").expect("fake executable");
        std::fs::write(temp.path().join(PORTABLE_FLAG_FILE), b"").expect("portable flag");

        assert_eq!(
            portable_root_from_executable(&exe),
            Some(temp.path().to_path_buf())
        );
    }

    #[test]
    fn gateway_portable_ignores_executable_without_flag() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exe = temp.path().join("LLM Gateway Desktop.exe");
        std::fs::write(&exe, b"").expect("fake executable");

        assert_eq!(portable_root_from_executable(&exe), None);
    }
}
'@
Write-Utf8 "src-tauri/src/portable.rs" $portableRs

Replace-Once "src-tauri/src/config.rs" @'
pub fn get_app_config_dir() -> PathBuf {
    if let Some(custom) = crate::app_store::get_app_config_dir_override() {
        return custom;
    }
'@ @'
pub fn get_app_config_dir() -> PathBuf {
    // A portable package is explicitly self-contained. The marker next to the
    // executable takes precedence over any path override saved by an installed
    // copy of the application on the same machine.
    if let Some(portable_dir) = crate::portable::data_dir() {
        return portable_dir;
    }

    if let Some(custom) = crate::app_store::get_app_config_dir_override() {
        return custom;
    }
'@ "crate::portable::data_dir()"

Replace-Once "src-tauri/src/settings.rs" @'
    fn settings_path() -> Option<PathBuf> {
        // settings.json 保留用于旧版本迁移和无数据库场景
        Some(
            crate::config::get_home_dir()
                .join(".llm-gateway-desktop")
                .join("settings.json"),
        )
    }
'@ @'
    fn settings_path() -> Option<PathBuf> {
        // settings.json follows the application data directory. In portable
        // mode this resolves to `<exe directory>/data/settings.json`; installed
        // builds continue to use the normal user data directory.
        Some(crate::config::get_app_config_dir().join("settings.json"))
    }
'@ "<exe directory>/data/settings.json"

Replace-Once "src-tauri/src/app_store.rs" @'
pub fn refresh_app_config_dir_override(app: &tauri::AppHandle) -> Option<PathBuf> {
    let value = read_override_from_store(app);
    update_cached_override(value.clone());
    value
}
'@ @'
pub fn refresh_app_config_dir_override(app: &tauri::AppHandle) -> Option<PathBuf> {
    if crate::portable::is_portable() {
        // Never inherit a path override from an installed copy. Portable mode
        // must remain anchored to the directory containing portable.flag.
        update_cached_override(None);
        return None;
    }

    let value = read_override_from_store(app);
    update_cached_override(value.clone());
    value
}
'@ "must remain anchored to the directory containing portable.flag"

Replace-Once "src-tauri/src/app_store.rs" @'
pub fn set_app_config_dir_to_store(
    app: &tauri::AppHandle,
    path: Option<&str>,
) -> Result<(), AppError> {
    let store = app
'@ @'
pub fn set_app_config_dir_to_store(
    app: &tauri::AppHandle,
    path: Option<&str>,
) -> Result<(), AppError> {
    if crate::portable::is_portable() {
        return Err(AppError::Message(
            "Portable mode always stores data in the local data directory".to_string(),
        ));
    }

    let store = app
'@ "Portable mode always stores data in the local data directory"

$lib = Read-Utf8 "src-tauri/src/lib.rs"
if (-not $lib.Contains("mod portable;")) {
    if (-not $lib.Contains("mod panic_hook;")) { throw "Could not locate panic_hook module declaration." }
    $lib = $lib.Replace("mod panic_hook;", "mod panic_hook;`nmod portable;")
}
if (-not $lib.Contains("portable::prepare_runtime()")) {
    $old = @'
pub fn run() {
    // 设置 panic hook，在应用崩溃时记录日志到 <app_config_dir>/crash.log（默认 ~/.llm-gateway-desktop/crash.log）
    panic_hook::setup_panic_hook();

    let mut builder = tauri::Builder::default();
'@
    $new = @'
pub fn run() {
    // Portable mode must be prepared before Tauri creates the Windows webview,
    // otherwise WebView2 may place browser data outside the portable folder.
    match portable::prepare_runtime() {
        Ok(Some(data_dir)) => panic_hook::init_app_config_dir(data_dir),
        Ok(None) => {}
        Err(error) => eprintln!("Failed to prepare portable data directory: {error}"),
    }

    // 设置 panic hook，在应用崩溃时记录日志到 <app_config_dir>/crash.log（默认 ~/.llm-gateway-desktop/crash.log）
    panic_hook::setup_panic_hook();

    let mut builder = tauri::Builder::default();
'@
    if (-not $lib.Contains($old)) { throw "Could not locate run() initialization block." }
    $lib = $lib.Replace($old, $new)
}

if (-not $lib.Contains("Store and window-state plugins persist files")) {
    $old = @'
    let builder = builder
        // 拦截窗口关闭：根据设置决定是否最小化到托盘
'@
    if ($lib.Contains($old)) {
        $lib = $lib.Replace($old, @'
    builder = builder
        // 拦截窗口关闭：根据设置决定是否最小化到托盘
'@)
    }

    $old = @'
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(window_state_flags())
                .build(),
        )
        .setup(|app| {
'@
    $new = @'
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init());

    // Store and window-state plugins persist files in the operating system app
    // data directory. They are useful for installed builds, but portable mode
    // deliberately avoids creating those external files.
    if !portable::is_portable() {
        builder = builder
            .plugin(tauri_plugin_store::Builder::new().build())
            .plugin(
                tauri_plugin_window_state::Builder::default()
                    .with_state_flags(window_state_flags())
                    .build(),
            );
    }

    let builder = builder.setup(|app| {
'@
    if (-not $lib.Contains($old)) { throw "Could not locate Tauri plugin chain." }
    $lib = $lib.Replace($old, $new)
}

if (-not $lib.Contains("Installed builds may use the saved path override")) {
    $old = @'
            // 预先刷新 Store 覆盖配置，确保后续路径读取正确（日志/数据库等）
            app_store::refresh_app_config_dir_override(app.handle());
            panic_hook::init_app_config_dir(crate::config::get_app_config_dir());
'@
    $new = @'
            // Installed builds may use the saved path override. Portable mode
            // intentionally skips the OS-level store and stays in local `data`.
            if !portable::is_portable() {
                app_store::refresh_app_config_dir_override(app.handle());
            }

            let app_config_dir = crate::config::get_app_config_dir();
            if let Err(error) = std::fs::create_dir_all(&app_config_dir) {
                return Err(error.into());
            }
            panic_hook::init_app_config_dir(app_config_dir.clone());
'@
    if (-not $lib.Contains($old)) { throw "Could not locate app data setup block." }
    $lib = $lib.Replace($old, $new)

    $oldDb = @'
            // 初始化数据库
            let app_config_dir = crate::config::get_app_config_dir();
            let db_path = app_config_dir.join("llm-gateway.db");
'@
    $newDb = @'
            // 初始化数据库
            let db_path = app_config_dir.join("llm-gateway.db");
'@
    if (-not $lib.Contains($oldDb)) { throw "Could not locate database path block." }
    $lib = $lib.Replace($oldDb, $newDb)
}

if (-not $lib.Contains("if portable::is_portable() {`n        return;")) {
    $old = @'
pub fn save_window_state_before_exit(app_handle: &tauri::AppHandle) {
    if let Err(err) = app_handle.save_window_state(window_state_flags()) {
'@
    $new = @'
pub fn save_window_state_before_exit(app_handle: &tauri::AppHandle) {
    if portable::is_portable() {
        return;
    }

    if let Err(err) = app_handle.save_window_state(window_state_flags()) {
'@
    if (-not $lib.Contains($old)) { throw "Could not locate window state save function." }
    $lib = $lib.Replace($old, $new)
}
Write-Utf8 "src-tauri/src/lib.rs" $lib
Write-Host "Updated: src-tauri/src/lib.rs"

$workflow = @'
name: Build and Release

on:
  workflow_dispatch:
  push:
    tags:
      - "v*"

permissions:
  contents: write

jobs:
  validate:
    name: Validate source
    runs-on: ubuntu-22.04
    steps:
      - name: Checkout
        uses: actions/checkout@v7

      - name: Install Linux build dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libwebkit2gtk-4.1-dev \
            libayatana-appindicator3-dev \
            librsvg2-dev \
            patchelf \
            xdg-utils \
            libssl-dev \
            libgtk-3-dev

      - name: Setup pnpm
        uses: pnpm/action-setup@v4
        with:
          version: 10.12.3
          run_install: false

      - name: Setup Node.js
        uses: actions/setup-node@v6
        with:
          node-version-file: .node-version
          cache: pnpm

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Rust cache
        uses: Swatinem/rust-cache@v2
        with:
          workspaces: "./src-tauri -> target"

      - name: Install frontend dependencies
        run: pnpm install --frozen-lockfile

      - name: TypeScript check
        run: pnpm typecheck

      - name: Gateway Rust tests
        run: cargo test --manifest-path src-tauri/Cargo.toml gateway --lib

  build:
    name: ${{ matrix.name }}
    needs: validate
    strategy:
      fail-fast: false
      matrix:
        include:
          - name: Windows x64
            platform: windows-2022
            args: "--bundles nsis,msi"
            rust_targets: ""

          - name: Linux x64
            platform: ubuntu-22.04
            args: "--bundles deb,appimage"
            rust_targets: ""

          - name: macOS Apple Silicon
            platform: macos-latest
            args: "--target aarch64-apple-darwin --bundles app,dmg"
            rust_targets: "aarch64-apple-darwin,x86_64-apple-darwin"

          - name: macOS Intel
            platform: macos-latest
            args: "--target x86_64-apple-darwin --bundles app,dmg"
            rust_targets: "aarch64-apple-darwin,x86_64-apple-darwin"

    runs-on: ${{ matrix.platform }}

    steps:
      - name: Checkout
        uses: actions/checkout@v7

      - name: Install Linux build dependencies
        if: matrix.platform == 'ubuntu-22.04'
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libwebkit2gtk-4.1-dev \
            libayatana-appindicator3-dev \
            librsvg2-dev \
            patchelf \
            xdg-utils \
            libssl-dev \
            libgtk-3-dev

      - name: Setup pnpm
        uses: pnpm/action-setup@v4
        with:
          version: 10.12.3
          run_install: false

      - name: Setup Node.js
        uses: actions/setup-node@v6
        with:
          node-version-file: .node-version
          cache: pnpm

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.rust_targets }}

      - name: Rust cache
        uses: Swatinem/rust-cache@v2
        with:
          workspaces: "./src-tauri -> target"
          key: ${{ matrix.name }}

      - name: Install frontend dependencies
        run: pnpm install --frozen-lockfile

      - name: Build Tauri packages
        uses: tauri-apps/tauri-action@v1
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          # Ad-hoc signing prevents Apple Silicon downloads from appearing damaged.
          # It is not notarization; users may still need to approve the app in
          # System Settings > Privacy & Security.
          APPLE_SIGNING_IDENTITY: ${{ startsWith(matrix.platform, 'macos-') && '-' || '' }}
        with:
          tauriScript: pnpm tauri
          args: ${{ matrix.args }}
          retryAttempts: 1

          # A manually-dispatched run only uploads workflow artifacts.
          # A v* tag additionally creates a draft GitHub Release.
          tagName: ${{ startsWith(github.ref, 'refs/tags/v') && github.ref_name || '' }}
          releaseName: ${{ startsWith(github.ref, 'refs/tags/v') && format('LLM Gateway Desktop {0}', github.ref_name) || '' }}
          releaseBody: ${{ startsWith(github.ref, 'refs/tags/v') && 'Cross-platform desktop packages. These builds are currently unsigned on Windows and ad-hoc signed on macOS.' || '' }}
          releaseDraft: true
          prerelease: ${{ contains(github.ref_name, 'alpha') || contains(github.ref_name, 'beta') || contains(github.ref_name, 'rc') }}
          generateReleaseNotes: false
          uploadUpdaterJson: false

          uploadWorkflowArtifacts: true
          workflowArtifactNamePattern: "[platform]-[arch]-[bundle]"
          releaseAssetNamePattern: "[name]_[version]_[platform]_[arch][setup].[ext]"

      - name: Create Windows portable ZIP
        if: matrix.platform == 'windows-2022'
        shell: pwsh
        run: |
          $version = (Get-Content "src-tauri/tauri.conf.json" -Raw | ConvertFrom-Json).version
          $portableRoot = Join-Path $PWD "portable"
          $packageDir = Join-Path $portableRoot "LLM Gateway Desktop"
          $exePath = "src-tauri/target/release/llm-gateway-desktop.exe"

          if (-not (Test-Path $exePath)) {
            throw "Portable executable was not found: $exePath"
          }

          New-Item -ItemType Directory -Force -Path $packageDir | Out-Null
          New-Item -ItemType Directory -Force -Path (Join-Path $packageDir "data") | Out-Null
          New-Item -ItemType File -Force -Path (Join-Path $packageDir "portable.flag") | Out-Null
          Copy-Item $exePath (Join-Path $packageDir "LLM Gateway Desktop.exe")
          Copy-Item "LICENSE" $packageDir
          Copy-Item "THIRD_PARTY_NOTICES.md" $packageDir
          Copy-Item "README.md" $packageDir

          @"
          LLM Gateway Desktop $version - Windows x64 Portable

          Extract the whole folder, then run LLM Gateway Desktop.exe.
          Do not run the executable directly from inside the ZIP archive.

          This package runs in portable mode because portable.flag is present.
          Database, settings, logs, crash reports and WebView2 browser data are
          stored in the local data folder next to the executable.

          Keep portable.flag beside the executable. Deleting it switches the
          application back to the normal per-user data directory on next launch.

          Microsoft Edge WebView2 Runtime is required. It is normally already
          installed on supported Windows 10 and Windows 11 systems.
          "@ | Set-Content (Join-Path $packageDir "PORTABLE_README.txt") -Encoding UTF8

          $zipName = "LLM-Gateway-Desktop_${version}_windows_x64_portable.zip"
          $zipPath = Join-Path $portableRoot $zipName
          Compress-Archive -Path $packageDir -DestinationPath $zipPath -CompressionLevel Optimal -Force
          Write-Host "Created portable package: $zipPath"

      - name: Upload Windows portable ZIP artifact
        if: matrix.platform == 'windows-2022'
        uses: actions/upload-artifact@v4
        with:
          name: windows-x64-portable
          path: portable/*.zip
          if-no-files-found: error

      - name: Upload Windows portable ZIP to Release
        if: matrix.platform == 'windows-2022' && startsWith(github.ref, 'refs/tags/v')
        shell: pwsh
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          $zip = Get-ChildItem "portable/*.zip" | Select-Object -First 1
          if (-not $zip) {
            throw "Portable ZIP was not created."
          }
          gh release upload "${{ github.ref_name }}" $zip.FullName --clobber
'@
Write-Utf8 ".github/workflows/build-release.yml" $workflow
Write-Host "Updated: .github/workflows/build-release.yml"

$readme = Read-Utf8 "README.md"
if (-not $readme.Contains("## Windows portable package")) {
    $readme = $readme.TrimEnd() + @'

## Windows portable package

Release builds include a Windows x64 portable ZIP in addition to NSIS and MSI installers.
Extract the entire folder and run `LLM Gateway Desktop.exe`. The included
`portable.flag` enables portable mode, so application-owned data is written to
`data/` beside the executable. Keep the folder writable and do not run the EXE
directly from inside the ZIP archive.

Removing `portable.flag` makes the same executable use the normal per-user data
directory on its next launch.
'@
    Write-Utf8 "README.md" $readme
    Write-Host "Updated: README.md"
}

Push-Location $root
try {
    git diff --check
    Write-Host ""
    Write-Host "Portable mode update applied successfully." -ForegroundColor Green
    Write-Host "Review with: git diff"
    Write-Host "Then commit, push, and run build-release.yml."
    git status --short
}
finally {
    Pop-Location
}
