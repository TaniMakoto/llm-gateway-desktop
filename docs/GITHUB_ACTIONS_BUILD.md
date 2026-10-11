# GitHub Actions multi-platform builds

The workflow `.github/workflows/build-release.yml` builds:

- Windows x64: NSIS `.exe` and WiX `.msi`
- Linux x64: `.deb` and `.AppImage`
- macOS Apple Silicon: `.app` and `.dmg`
- macOS Intel: `.app` and `.dmg`

## Manual test build

Run:

```powershell
gh workflow run build-release.yml
gh run list --workflow build-release.yml --event workflow_dispatch --limit 1 --json databaseId,url,headSha
```

Manual runs upload packages to the workflow's **Artifacts** section but do not create a Release.

For Windows-only updates, dispatch `package-windows.yml` with `--ref` set to the
branch containing the fix. That workflow runs TypeScript and formatting checks,
independent `gateway-core` tests, and the full Rust library test suite (including
the real-HTTP protocol matrix) before producing
the installers and `windows-x64-portable` artifact.

Agents must finish local checks before pushing and dispatching a build. After
dispatch, report the commit, workflow link, actual local check results and checks
delegated to CI, then stop. Do not watch or repeatedly poll the run unless the
user explicitly asks to check its result. A triggered workflow does not confirm
a successful build or installation. Local executable replacement is a separate
step after a successful build; do not report it as completed at dispatch time.

## Create a versioned release

Keep these versions identical before tagging:

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`

Then run:

```powershell
git tag v0.2.0
git push origin v0.2.0
gh run list --workflow build-release.yml --event push --limit 1 --json databaseId,url,headSha
```

The workflow creates a **draft** Release. After all four build jobs pass:

```powershell
gh release edit v0.2.0 --draft=false
gh release view v0.2.0 --web
```

## Signing status

- Windows packages are unsigned and can trigger Microsoft SmartScreen.
- macOS packages use ad-hoc signing, not Apple notarization. Users may need to approve the application under Privacy & Security.
- Linux packages do not require signing for basic testing and distribution.
