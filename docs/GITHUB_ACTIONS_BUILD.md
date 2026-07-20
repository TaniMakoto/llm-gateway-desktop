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
$runId = gh run list --workflow build-release.yml --limit 1 --json databaseId --jq '.[0].databaseId'
gh run watch $runId --exit-status
gh run download $runId -D artifacts
```

Manual runs upload packages to the workflow's **Artifacts** section but do not create a Release.

## Create a versioned release

Keep these versions identical before tagging:

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`

Then run:

```powershell
git tag v0.1.0
git push origin v0.1.0
$runId = gh run list --workflow build-release.yml --limit 1 --json databaseId --jq '.[0].databaseId'
gh run watch $runId --exit-status
```

The workflow creates a **draft** Release. After all four build jobs pass:

```powershell
gh release edit v0.1.0 --draft=false
gh release view v0.1.0 --web
```

## Signing status

- Windows packages are unsigned and can trigger Microsoft SmartScreen.
- macOS packages use ad-hoc signing, not Apple notarization. Users may need to approve the application under Privacy & Security.
- Linux packages do not require signing for basic testing and distribution.
