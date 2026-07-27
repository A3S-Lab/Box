[CmdletBinding()]
param(
    [string]$A3sBoxPath,
    [string]$ShimPath,
    [string]$KrunDllPath,
    [string]$FirmwareDllPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$installer = Join-Path $repositoryRoot 'install.ps1'
if ([string]::IsNullOrWhiteSpace($A3sBoxPath)) {
    $A3sBoxPath = Join-Path $repositoryRoot 'src/target/debug/a3s-box.exe'
}
if ([string]::IsNullOrWhiteSpace($ShimPath)) {
    $ShimPath = Join-Path $repositoryRoot 'src/target/debug/a3s-box-shim.exe'
}
$prebuiltRoot = Join-Path $repositoryRoot (
    'src/deps/libkrun-sys/prebuilt/x86_64-pc-windows-msvc'
)
if ([string]::IsNullOrWhiteSpace($KrunDllPath)) {
    $KrunDllPath = Join-Path $prebuiltRoot 'krun.dll'
}
if ([string]::IsNullOrWhiteSpace($FirmwareDllPath)) {
    $FirmwareDllPath = Join-Path $prebuiltRoot 'libkrunfw.dll'
}

foreach ($source in @($A3sBoxPath, $ShimPath, $KrunDllPath, $FirmwareDllPath)) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Installer fixture source is missing: $source"
    }
}

$versionOutput = @(& $A3sBoxPath --version 2>&1)
if ($LASTEXITCODE -ne 0) {
    throw "Unable to query fixture version: $($versionOutput -join ' ')"
}
$versionMatch = [regex]::Match(
    ($versionOutput -join [Environment]::NewLine),
    '(?m)^a3s-box\s+(\S+)'
)
if (-not $versionMatch.Success) {
    throw "Unexpected fixture version output: $($versionOutput -join ' ')"
}
$version = $versionMatch.Groups[1].Value
$tag = "v$version"
$platform = 'windows-x86_64'
$packageName = "a3s-box-$tag-$platform"

$testRoot = Join-Path ([IO.Path]::GetTempPath()) (
    'a3s-box-installer-test-{0}' -f [Guid]::NewGuid().ToString('N')
)
$temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$resolvedTestRoot = [IO.Path]::GetFullPath($testRoot)
if (-not $resolvedTestRoot.StartsWith(
    $temporaryRoot,
    [StringComparison]::OrdinalIgnoreCase
)) {
    throw "Refusing to use a test directory outside the temporary root: $resolvedTestRoot"
}
New-Item -ItemType Directory -Path $testRoot | Out-Null

function Assert-ThrowsContaining {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$Expected
    )

    try {
        & $Action
    }
    catch {
        if (-not $_.Exception.Message.Contains($Expected)) {
            throw (
                "Expected failure containing '$Expected', found: " +
                $_.Exception.Message
            )
        }
        return
    }
    throw "Expected action to fail with text: $Expected"
}

try {
    $packageRoot = Join-Path $testRoot $packageName
    New-Item -ItemType Directory -Path $packageRoot | Out-Null
    Copy-Item -LiteralPath $A3sBoxPath `
        -Destination (Join-Path $packageRoot 'a3s-box.exe')
    Copy-Item -LiteralPath $ShimPath `
        -Destination (Join-Path $packageRoot 'a3s-box-shim.exe')
    Copy-Item -LiteralPath $KrunDllPath `
        -Destination (Join-Path $packageRoot 'krun.dll')
    Copy-Item -LiteralPath $FirmwareDllPath `
        -Destination (Join-Path $packageRoot 'libkrunfw.dll')
    Set-Content -LiteralPath (Join-Path $packageRoot 'a3s-box-guest-init') `
        -Value 'fixture guest executable' -Encoding Ascii

    $archive = Join-Path $testRoot "$packageName.zip"
    Compress-Archive -LiteralPath $packageRoot -DestinationPath $archive
    $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash

    $happyInstall = Join-Path $testRoot 'happy-install'
    & $installer -Version $tag -ArchivePath $archive -Sha256 $digest `
        -InstallDir $happyInstall -NoModifyPath
    $marker = Get-Content `
        -LiteralPath (Join-Path $happyInstall '.a3s-box-install.json') `
        -Raw | ConvertFrom-Json
    if ($marker.version -cne $tag -or $marker.platform -cne $platform) {
        throw 'Installer ownership marker has the wrong version or platform.'
    }
    $installedOutput = @(& (Join-Path $happyInstall 'a3s-box.exe') --version)
    if (($installedOutput -join ' ') -notmatch [regex]::Escape("a3s-box $version")) {
        throw 'Installed Windows fixture did not run.'
    }

    Set-Content -LiteralPath (Join-Path $happyInstall 'stale.file') `
        -Value stale -Encoding Ascii
    & $installer -Version $tag -ArchivePath $archive -Sha256 $digest `
        -InstallDir $happyInstall -NoModifyPath
    if (Test-Path -LiteralPath (Join-Path $happyInstall 'stale.file')) {
        throw 'Managed reinstall retained a stale file.'
    }

    $tamperedArchive = Join-Path $testRoot 'tampered.zip'
    Copy-Item -LiteralPath $archive -Destination $tamperedArchive
    Add-Content -LiteralPath $tamperedArchive -Value tampered -Encoding Ascii
    $tamperedInstall = Join-Path $testRoot 'tampered-install'
    Assert-ThrowsContaining -Expected 'SHA-256 mismatch' -Action {
        & $installer -Version $tag -ArchivePath $tamperedArchive `
            -Sha256 $digest -InstallDir $tamperedInstall -NoModifyPath
    }
    if (Test-Path -LiteralPath $tamperedInstall) {
        throw 'Digest failure modified the installation destination.'
    }

    $unmanagedInstall = Join-Path $testRoot 'unmanaged-install'
    New-Item -ItemType Directory -Path $unmanagedInstall | Out-Null
    Set-Content -LiteralPath (Join-Path $unmanagedInstall 'unmanaged.file') `
        -Value keep -Encoding Ascii
    Assert-ThrowsContaining -Expected 'is not managed by this installer' -Action {
        & $installer -Version $tag -ArchivePath $archive -Sha256 $digest `
            -InstallDir $unmanagedInstall -NoModifyPath
    }
    if (-not (Test-Path -LiteralPath (Join-Path $unmanagedInstall 'unmanaged.file'))) {
        throw 'Unmanaged directory changed after a refused install.'
    }
    & $installer -Version $tag -ArchivePath $archive -Sha256 $digest `
        -InstallDir $unmanagedInstall -NoModifyPath -Force
    if (Test-Path -LiteralPath (Join-Path $unmanagedInstall 'unmanaged.file')) {
        throw '-Force retained an unmanaged file.'
    }

    $wrongPackage = Join-Path $testRoot 'wrong-root'
    New-Item -ItemType Directory -Path $wrongPackage | Out-Null
    Set-Content -LiteralPath (Join-Path $wrongPackage 'file') `
        -Value wrong -Encoding Ascii
    $wrongArchive = Join-Path $testRoot 'wrong-root.zip'
    Compress-Archive -LiteralPath $wrongPackage -DestinationPath $wrongArchive
    $wrongDigest = (
        Get-FileHash -LiteralPath $wrongArchive -Algorithm SHA256
    ).Hash
    $wrongInstall = Join-Path $testRoot 'wrong-install'
    Assert-ThrowsContaining -Expected 'unsafe or unexpected path' -Action {
        & $installer -Version $tag -ArchivePath $wrongArchive `
            -Sha256 $wrongDigest -InstallDir $wrongInstall -NoModifyPath
    }
    if (Test-Path -LiteralPath $wrongInstall) {
        throw 'Layout failure modified the installation destination.'
    }

    $protectedDestination = [Environment]::GetFolderPath(
        [Environment+SpecialFolder]::UserProfile
    )
    Assert-ThrowsContaining -Expected 'protected system directory' -Action {
        & $installer -Version $tag -ArchivePath $archive -Sha256 $digest `
            -InstallDir $protectedDestination -NoModifyPath -Force
    }

    # Exercise the exact execution shape used by `irm ... | iex`. Environment
    # defaults make that path configurable without editing the downloaded text.
    $pipelineInstall = Join-Path $testRoot 'pipeline-install'
    $expectedPipelineVersion = $version
    $environmentValues = @{
        A3S_BOX_VERSION        = $tag
        A3S_BOX_INSTALL_DIR    = $pipelineInstall
        A3S_BOX_ARCHIVE        = $archive
        A3S_BOX_SHA256         = $digest
        A3S_BOX_NO_MODIFY_PATH = 'true'
    }
    $previousEnvironment = @{}
    foreach ($name in $environmentValues.Keys) {
        $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable(
            $name,
            'Process'
        )
        [Environment]::SetEnvironmentVariable(
            $name,
            $environmentValues[$name],
            'Process'
        )
    }
    try {
        $installerText = Get-Content -LiteralPath $installer -Raw
        $installerText | Invoke-Expression
    }
    finally {
        foreach ($name in $environmentValues.Keys) {
            [Environment]::SetEnvironmentVariable(
                $name,
                $previousEnvironment[$name],
                'Process'
            )
        }
    }
    $pipelineOutput = @(& (Join-Path $pipelineInstall 'a3s-box.exe') --version)
    if (
        ($pipelineOutput -join ' ') -notmatch
        [regex]::Escape("a3s-box $expectedPipelineVersion")
    ) {
        throw 'The Invoke-Expression installation path did not run.'
    }

    Write-Output "install.ps1 tests passed for $platform"
}
finally {
    if (Test-Path -LiteralPath $resolvedTestRoot) {
        Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
    }
}
