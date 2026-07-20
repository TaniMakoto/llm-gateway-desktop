# 发布到公开 GitHub 仓库

以下命令适用于已经登录 GitHub CLI 的 PowerShell。

## 1. 创建公开仓库

在源码根目录执行：

```powershell
gh auth status
git init -b main
git add .
git commit -m "Initial public alpha"
gh repo create llm-gateway-desktop `
  --public `
  --source=. `
  --remote=origin `
  --push
```

如仓库名已被占用，将 `llm-gateway-desktop` 换成其他名称即可。仓库名不会影响应用内部运行。

## 2. 运行第一次云端构建

```powershell
gh workflow run build-release.yml --ref main
Start-Sleep -Seconds 5
$runId = gh run list `
  --workflow build-release.yml `
  --event workflow_dispatch `
  --limit 1 `
  --json databaseId `
  --jq '.[0].databaseId'
gh run watch $runId --compact --exit-status
```

成功后下载 artifacts：

```powershell
gh run download $runId -D .rtifacts
```

若失败，导出完整失败日志：

```powershell
gh run view $runId --log-failed |
  Out-File -Encoding utf8 .uild-failed.log
```

第一次构建成功前不要创建正式标签。

## 3. 创建 alpha Release

```powershell
git tag v0.1.0-alpha.1
git push origin v0.1.0-alpha.1
```

标签构建会创建草稿 Release。检查四个平台文件后发布：

```powershell
gh release edit v0.1.0-alpha.1 `
  --draft=false `
  --prerelease=true
gh release view v0.1.0-alpha.1 --web
```

## 4. 发布前检查

- GitHub Actions 四个平台任务全部通过。
- 仓库中没有真实 API Key、数据库、日志或 `.env`。
- `LICENSE` 与 `THIRD_PARTY_NOTICES.md` 保持存在。
- Release 说明明确标注 alpha、Windows 未签名、macOS 未公证。
- 至少在 Windows 上实际启动一次安装包并完成 Mock 请求测试。
