# 发布新的 a3s-box Windows 版本到 winget

本页是快速清单；完整说明见 [WINGET_PUBLISHING.md](WINGET_PUBLISHING.md)。

> `.winget/` 中提交到仓库的版本、URL 和 SHA256 只是供发布工具重写的
> seed 值，不代表对应 GitHub Release 已存在。只有在目标 tag 的 Windows ZIP
> 已成功上传后才能发布 manifest。不要把仓库中的 `0.8.0` seed 当成可发布版本，
> 也不要为缺少 Windows ZIP 的现有 release 伪造哈希。

## 1. 确认发布资产

把 `<VERSION>` 替换为准备发布的实际版本号。它必须是严格的 SemVer 2.0.0，
且不含前导 `v`（例如 `3.0.12`，不能写成 `v3.0.12`）：

```powershell
$Version = "<VERSION>"
$Tag = "v$Version"
$Asset = "a3s-box-$Tag-windows-x86_64.zip"
$Url = "https://github.com/A3S-Lab/Box/releases/download/$Tag/$Asset"

# 失败或返回 404 时停止，先完成 Windows release。
Invoke-WebRequest -Uri $Url -OutFile $Asset
(Get-FileHash -Path $Asset -Algorithm SHA256).Hash
```

ZIP 的顶层版本目录中必须包含：

```text
a3s-box-v<VERSION>-windows-x86_64/
├── a3s-box.exe
├── a3s-box-shim.exe
├── a3s-box-guest-init
├── krun.dll
└── libkrunfw.dll
```

两个 DLL 必须与 Windows 可执行文件同级。

运行 a3s-box 的账户还必须启用 Windows Developer Mode，或拥有
`SeCreateSymbolicLinkPrivilege`，以便 OCI 层中的符号链接能被正确解压。

## 2. 运行发布工作流（推荐）

1. 打开 [Publish to winget](https://github.com/A3S-Lab/Box/actions/workflows/publish-winget.yml)。
2. 选择 **Run workflow**。
3. 输入实际的 `<VERSION>`。
4. 运行工作流并确认下载、哈希计算和 manifest 验证全部成功。
5. 如果自动提交失败，下载工作流生成的 `winget-manifests` artifact，按 job
   summary 中的步骤完成手动 PR。

自动提交只需要仓库 secret `WINGET_TOKEN`。工作流使用固定提交 SHA 的 GitHub
Actions，并下载固定版本的 WinGetCreate、校验官方发布的 SHA256、执行该工具的
`info` 自检，再使用 `winget validate --manifest` 实际验证 manifest。验证成功后，
由已校验的 WinGetCreate 执行提交。

## 3. 使用本地脚本

本地环境需要可用的 `winget validate` 命令和 .NET 8 Runtime（用于运行固定版本的
WinGetCreate）。

```powershell
$env:GITHUB_TOKEN = "<GITHUB_TOKEN>"
.\scripts\submit-to-winget.ps1 -Version "<VERSION>"
```

脚本会下载目标 release 的 Windows ZIP、计算真实 SHA256、重写 `.winget/`
中的 seed 值、下载并校验固定版本的 WinGetCreate、验证 manifest，并尝试创建
`winget-pkgs` PR。任何下载、工具校验或 manifest 验证失败都应视为发布阻断，
不能手工填入猜测值继续。只验证而不提交时使用：

```powershell
.\scripts\submit-to-winget.ps1 -Version "<VERSION>" -ValidateOnly
```

## 4. 手动提交

如果自动提交不可用：

1. 确认 `.winget/` 中三个 manifest 的 `PackageVersion` 完全一致。
2. 确认 `InstallerUrl` 指向刚刚下载并校验过的 Windows ZIP。
3. 确认 `InstallerSha256` 来自该 ZIP，而不是模板或另一版本。
4. 运行 `winget validate --manifest .winget\ --disable-interactivity`。
5. 将三个 YAML 文件复制到 fork 的
   `manifests/a/A3SLab/Box/<VERSION>/` 并向
   [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) 创建 PR。

合并后可验证：

```powershell
winget search A3SLab.Box
winget show A3SLab.Box
winget install A3SLab.Box
```
