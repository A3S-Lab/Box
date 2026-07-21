# 发布到 Windows Package Manager (winget)

本文档说明如何将 a3s-box 发布到 Windows Package Manager (winget)。

> `.winget/` 中提交到仓库的 `0.8.0`、URL 与 SHA256 是发布工具会重写的
> seed 值，并不表示该 release 存在。发布时必须输入一个已经包含
> `a3s-box-v<VERSION>-windows-x86_64.zip` 的实际 tag；不能为缺少 Windows
> ZIP 的 release 复用模板哈希。

## 方法 1: GitHub Actions 发布 (推荐)

### 前提条件

1. **创建 GitHub Personal Access Token**
   - 访问 https://github.com/settings/tokens
   - 创建 token，权限: `public_repo`
   - 保存 token

2. **配置 GitHub Secrets**
   - 在仓库设置中添加 secrets:
     - `WINGET_TOKEN`: 你的 GitHub token

### 触发发布

发布工作流当前需要手动运行：

```bash
# 在 GitHub Actions 页面手动触发
# Actions -> Publish to winget -> Run workflow
```

输入必须是严格的 SemVer 2.0.0，且不能带前导 `v`。例如输入 `3.0.12`，
而不是 `v3.0.12`。工作流先验证该输入，再构造唯一的 `v<VERSION>` tag，
因此不会接受重复 `v` 或把输入内容直接插入脚本。

工作流把所有 GitHub Actions 固定到完整 commit SHA，并使用 WinGetCreate
`1.12.8.0`。它从 Microsoft 官方 GitHub release 下载可执行文件，校验 SHA256
`8BD738851B524885410112678E3771B341C5C716DE60FBBECB88AB0A363ED85D` 后执行
该工具的 `info` 自检。manifest 由 `winget validate --manifest` 实际验证；只有
验证成功后，已校验的 WinGetCreate 才会执行 `submit`。

## 方法 2: 使用 PowerShell 脚本

本地环境需要可用的 `winget validate` 命令和 .NET 8 Runtime（用于运行固定版本的
WinGetCreate）。

```powershell
# 设置 GitHub token
$env:GITHUB_TOKEN = "your_github_token_here"

# 运行提交脚本
.\scripts\submit-to-winget.ps1 -Version "<VERSION>"
```

脚本会自动：
1. 下载 Windows 发布资产
2. 计算 SHA256 哈希
3. 更新 manifest 文件
4. 验证 manifest
5. 创建 PR 到 microsoft/winget-pkgs

如只需更新并验证 manifest、不要提交 PR，可运行：

```powershell
.\scripts\submit-to-winget.ps1 -Version "<VERSION>" -ValidateOnly
```

## 方法 3: 手动提交

### 步骤 1: 准备 manifest 文件

manifest 文件位于 `.winget/` 目录：
- `A3SLab.Box.yaml` - 版本清单
- `A3SLab.Box.installer.yaml` - 安装程序信息
- `A3SLab.Box.locale.en-US.yaml` - 本地化信息

### 步骤 2: 更新版本和 SHA256

```powershell
# 下载发布资产
$Version = "<VERSION>"
$Tag = "v$Version"
$Url = "https://github.com/A3S-Lab/Box/releases/download/$Tag/a3s-box-$Tag-windows-x86_64.zip"
Invoke-WebRequest -Uri $Url -OutFile "a3s-box.zip"

# 计算 SHA256
$Hash = Get-FileHash -Path "a3s-box.zip" -Algorithm SHA256
$SHA256 = $Hash.Hash
Write-Host "SHA256: $SHA256"
```

ZIP 中必须保留顶层版本目录，并让两个 DLL 与 Windows 可执行文件同级：

```text
a3s-box-v0.8.0-windows-x86_64/
├── a3s-box.exe
├── a3s-box-shim.exe
├── a3s-box-guest-init
├── krun.dll
└── libkrunfw.dll
```

不要把 DLL 移到 `lib/` 子目录；Windows 加载器需要在可执行文件目录中找到
`krun.dll`，而 `libkrunfw.dll` 也必须与 `krun.dll` 相邻。

手动更新 `.winget/A3SLab.Box.installer.yaml`:
- `PackageVersion`: 更新为新版本
- `InstallerUrl`: 更新 URL
- `InstallerSha256`: 更新为计算的 SHA256
- `RelativeFilePath`: 更新路径中的版本号

### 步骤 3: 验证 manifest

```powershell
# 脚本会校验固定版本的 WinGetCreate，并实际执行 winget validate --manifest。
.\scripts\submit-to-winget.ps1 -Version "<VERSION>" -ValidateOnly
```

### 步骤 4: 提交到 winget-pkgs

#### 选项 A: 使用 wingetcreate (推荐)

```powershell
$env:GITHUB_TOKEN = "YOUR_GITHUB_TOKEN"
.\scripts\submit-to-winget.ps1 -Version "<VERSION>"
```

#### 选项 B: 手动创建 PR

1. Fork https://github.com/microsoft/winget-pkgs
2. 创建目录: `manifests/a/A3SLab/Box/0.8.0/`
3. 复制 manifest 文件到该目录
4. 提交并创建 PR

## Manifest 文件说明

### A3SLab.Box.yaml (版本清单)
```yaml
PackageIdentifier: A3SLab.Box
PackageVersion: 0.8.0
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.9.0
```

### A3SLab.Box.installer.yaml (安装程序)
```yaml
PackageIdentifier: A3SLab.Box
PackageVersion: 0.8.0
Platform:
- Windows.Desktop
MinimumOSVersion: 10.0.19041.0
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: a3s-box-v0.8.0-windows-x86_64\a3s-box.exe
  PortableCommandAlias: a3s-box
- RelativeFilePath: a3s-box-v0.8.0-windows-x86_64\a3s-box-shim.exe
ArchiveBinariesDependOnPath: true
Installers:
- Architecture: x64
  InstallerUrl: https://github.com/A3S-Lab/Box/releases/download/v0.8.0/a3s-box-v0.8.0-windows-x86_64.zip
  InstallerSha256: <COMPUTED_SHA256>
  Dependencies:
    WindowsFeatures:
    - HypervisorPlatform
ManifestType: installer
ManifestVersion: 1.9.0
```

### A3SLab.Box.locale.en-US.yaml (本地化)
包含包的描述、标签、发布说明等信息。

## 验证发布

发布成功后，用户可以通过以下命令安装：

```powershell
# 搜索包
winget search a3s-box

# 安装
winget install A3SLab.Box

# 升级
winget upgrade A3SLab.Box
```

## 注意事项

1. **Windows Feature 依赖**: Windows 包使用原生 WHPX 后端，不依赖 WSL。用户需要启用 Windows Hypervisor Platform：
   ```powershell
   Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
   ```

   运行 a3s-box 的账户还必须启用 Windows Developer Mode，或拥有
   `SeCreateSymbolicLinkPrivilege`，否则 OCI 层中的符号链接无法被完整还原。

2. **Portable 安装**: 使用 `portable` 类型和 `ArchiveBinariesDependOnPath: true`，winget 会保留归档中的 guest-init 与 DLL，并将安装目录添加到 PATH。`NestedInstallerFiles` 只列出两个 Windows 可执行文件，不能列出 DLL。

3. **审核时间**: PR 提交后，winget 维护者会审核，通常需要 1-3 天。

4. **版本更新**: 每次发布新版本都需要提交新的 manifest。

## 故障排除

### SHA256 不匹配
确保下载的文件完整，重新计算 SHA256。

### Manifest 验证失败
运行 `winget validate --manifest .winget\ --disable-interactivity` 查看详细错误信息。

### PR 被拒绝
查看 PR 评论，根据维护者反馈修改 manifest。

## 参考资料

- [winget 官方文档](https://learn.microsoft.com/en-us/windows/package-manager/)
- [winget-pkgs 仓库](https://github.com/microsoft/winget-pkgs)
- [wingetcreate 工具](https://github.com/microsoft/winget-create)
- [Manifest 规范](https://github.com/microsoft/winget-pkgs/tree/master/doc/manifest)
