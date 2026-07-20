$ErrorActionPreference = 'Stop'

function Replace-Text {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Old,
        [Parameter(Mandatory = $true)][string]$New
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "File not found: $Path"
    }

    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    $text = [System.IO.File]::ReadAllText((Resolve-Path -LiteralPath $Path))
    if (-not $text.Contains($Old)) {
        Write-Host "Already clean or not present: $Old"
        return
    }

    $text = $text.Replace($Old, $New)
    [System.IO.File]::WriteAllText((Resolve-Path -LiteralPath $Path), $text, $utf8NoBom)
    Write-Host "Replaced in $Path"
}

Replace-Text 'src-tauri/src/services/subscription.rs' `
    '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' `
    'TEST_GOOGLE_OAUTH_CLIENT_SECRET'

Replace-Text 'src-tauri/src/services/s3.rs' `
    'AKIAIOSFODNN7EXAMPLE' `
    'TEST_AWS_ACCESS_KEY_ID'

Replace-Text 'src-tauri/src/proxy/providers/auth.rs' `
    'sk-1234567890abcdef' `
    'test-api-key-1234567890abcdef'

Replace-Text 'src-tauri/src/proxy/providers/codex.rs' `
    'sk-test-key-12345678' `
    'test-openai-key-12345678'
Replace-Text 'src-tauri/src/proxy/providers/codex.rs' `
    'sk-env-key-12345678' `
    'test-env-key-12345678'
Replace-Text 'src-tauri/src/proxy/providers/codex.rs' `
    'sk-anthropic-key-123' `
    'test-anthropic-key-123'

Replace-Text 'src-tauri/src/proxy/providers/claude.rs' `
    'sk-ant-test-key' `
    'test-anthropic-key'
Replace-Text 'src-tauri/src/proxy/providers/claude.rs' `
    'sk-from-auth-token' `
    'test-from-auth-token'
Replace-Text 'src-tauri/src/proxy/providers/claude.rs' `
    'sk-from-api-key' `
    'test-from-api-key'

Write-Host ''
Write-Host 'Potential high-confidence secret patterns remaining:'
$patterns = 'GOCSPX-|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{25,}|github_pat_|gh[pousr]_|-----BEGIN .*PRIVATE KEY-----'
git grep -n -E $patterns 2>$null
if ($LASTEXITCODE -eq 1) {
    Write-Host 'None found.'
}

Write-Host ''
Write-Host 'Next commands:'
Write-Host '  git diff --check'
Write-Host '  git add -A'
Write-Host '  git commit --amend --no-edit'
Write-Host '  git push -u origin main'
