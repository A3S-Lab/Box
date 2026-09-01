#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = $env:A3S_BOX_VERSION,
    [string]$InstallDir = $env:A3S_BOX_INSTALL_DIR,
    [string]$ArchivePath = $env:A3S_BOX_ARCHIVE,
    [string]$Sha256 = $env:A3S_BOX_SHA256,
    [switch]$NoModifyPath = (
        [string]$env:A3S_BOX_NO_MODIFY_PATH -match '^(1|true|yes)$'
    ),
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repository = 'A3S-Lab/Box'
$apiRoot = "https://api.github.com/repos/$repository"
$releaseRoot = "https://github.com/$repository/releases/download"
$platform = 'windows-x86_64'
$markerName = '.a3s-box-install.json'
$temporaryDirectory = $null
$stageDirectory = $null
$backupContainer = $null

function Write-InstallerMessage {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host "a3s-box installer: $Message"
}

function Write-InstallerWarning {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Warning "a3s-box installer: $Message"
}

function Get-NormalizedTag {
    param([Parameter(Mandatory = $true)][string]$Candidate)

    $tag = if ($Candidate.StartsWith('v', [StringComparison]::Ordinal)) {
        $Candidate
    }
    else {
        "v$Candidate"
    }
    $number = $tag.Substring(1)
    if (
        [string]::IsNullOrWhiteSpace($number) -or
        $number -notmatch '^[0-9A-Za-z.+-]+$' -or
        $number -notmatch '^[^.]+\.[^.]+\.[^.]+'
    ) {
        throw "Invalid version '$Candidate'; expected a release such as v3.2.2."
    }
    return $tag
}

function Assert-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Digest)

    if ($Digest -notmatch '^[0-9A-Fa-f]{64}$') {
        throw 'SHA-256 must contain exactly 64 hexadecimal characters.'
    }
}

function Get-GitHubHeaders {
    $headers = @{
        Accept                  = 'application/vnd.github+json'
        'X-GitHub-Api-Version' = '2022-11-28'
    }
    if (-not [string]::IsNullOrWhiteSpace($env:GITHUB_TOKEN)) {
        $headers.Authorization = "Bearer $($env:GITHUB_TOKEN)"
    }
    return $headers
}

function Invoke-ReleaseDownload {
    param(
        [Parameter(Mandatory = $true)][uri]$Uri,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Destination
}

function Get-SidecarDigest {
    param(
        [Parameter(Mandatory = $true)][string]$Manifest,
        [Parameter(Mandatory = $true)][string]$AssetName
    )

    $digestCandidates = @()
    foreach ($line in Get-Content -LiteralPath $Manifest) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$') {
            if ([string]::Equals($Matches[2], $AssetName, [StringComparison]::Ordinal)) {
                $digestCandidates += $Matches[1]
            }
        }
    }
    if ($digestCandidates.Count -eq 1) {
        return $digestCandidates[0]
    }
    return $null
}

function Assert-ArchiveLayout {
    param(
        [Parameter(Mandatory = $true)][string]$Archive,
        [Parameter(Mandatory = $true)][string]$ExpectedRoot
    )

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        if ($zip.Entries.Count -eq 0) {
            throw 'Release archive is empty.'
        }
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            $segments = @($name.Split('/') | Where-Object { $_ -ne '' })
            if (
                $name.StartsWith('/', [StringComparison]::Ordinal) -or
                $segments -contains '..' -or
                $segments -contains '.' -or
                -not (
                    [string]::Equals($name, $ExpectedRoot, [StringComparison]::Ordinal) -or
                    $name.StartsWith("$ExpectedRoot/", [StringComparison]::Ordinal)
                )
            ) {
                throw "Release archive contains an unsafe or unexpected path: $name"
            }
        }
    }
    finally {
        $zip.Dispose()
    }
}

function Assert-InstalledVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion
    )

    $output = @(& $Executable --version 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "Installed a3s-box failed its version check: $($output -join [Environment]::NewLine)"
    }
    $text = $output -join [Environment]::NewLine
    $match = [regex]::Match($text, '(?m)^a3s-box\s+(\S+)')
    if (-not $match.Success) {
        throw "Unexpected a3s-box --version output: $text"
    }
    $actualVersion = $match.Groups[1].Value
    if (-not [string]::Equals(
        $actualVersion,
        $ExpectedVersion,
        [StringComparison]::Ordinal
    )) {
        throw "Archive reports a3s-box $actualVersion, expected $ExpectedVersion."
    }
}

function Add-InstallDirectoryToUserPath {
    param([Parameter(Mandatory = $true)][string]$Directory)

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @(
        ([string]$userPath).Split(';') |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    $alreadyPresent = $false
    foreach ($entry in $entries) {
        if ([string]::Equals(
            $entry.TrimEnd('\'),
            $Directory.TrimEnd('\'),
            [StringComparison]::OrdinalIgnoreCase
        )) {
            $alreadyPresent = $true
            break
        }
    }

    if (-not $alreadyPresent) {
        $newUserPath = if ([string]::IsNullOrWhiteSpace($userPath)) {
            $Directory
        }
        else {
            "$($userPath.TrimEnd(';'));$Directory"
        }
        [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
        Write-InstallerMessage "added $Directory to the user PATH"
    }

    $processEntries = @($env:Path.Split(';'))
    $inProcessPath = $processEntries | Where-Object {
        [string]::Equals(
            $_.TrimEnd('\'),
            $Directory.TrimEnd('\'),
            [StringComparison]::OrdinalIgnoreCase
        )
    }
    if (-not $inProcessPath) {
        $env:Path = "$Directory;$env:Path"
    }
}

if ($env:OS -ne 'Windows_NT') {
    throw 'This installer supports Windows only; use install.sh on Linux or macOS.'
}

$architecture = if (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
    $env:PROCESSOR_ARCHITEW6432
}
else {
    $env:PROCESSOR_ARCHITECTURE
}
if (-not [string]::Equals($architecture, 'AMD64', [StringComparison]::OrdinalIgnoreCase)) {
    throw "Windows architecture '$architecture' is unsupported; A3S Box requires x86_64."
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $localApplicationData = [Environment]::GetFolderPath(
        [Environment+SpecialFolder]::LocalApplicationData
    )
    if ([string]::IsNullOrWhiteSpace($localApplicationData)) {
        throw 'Local application data is unavailable; specify -InstallDir.'
    }
    $InstallDir = Join-Path $localApplicationData 'Programs\A3S Box'
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$volumeRoot = [IO.Path]::GetPathRoot($InstallDir)
if ([string]::Equals(
    $InstallDir.TrimEnd('\'),
    $volumeRoot.TrimEnd('\'),
    [StringComparison]::OrdinalIgnoreCase
)) {
    throw 'Refusing to install at a volume root.'
}

$userProfileDirectory = [Environment]::GetFolderPath(
    [Environment+SpecialFolder]::UserProfile
)
$protectedDestinations = @(
    $volumeRoot,
    [Environment]::GetFolderPath([Environment+SpecialFolder]::Windows),
    [Environment]::GetFolderPath([Environment+SpecialFolder]::System),
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles),
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFilesX86),
    $userProfileDirectory,
    [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData),
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData),
    [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)
)
if (-not [string]::IsNullOrWhiteSpace($userProfileDirectory)) {
    $protectedDestinations += Split-Path -Parent $userProfileDirectory
}
foreach ($protectedDestination in $protectedDestinations) {
    if ([string]::IsNullOrWhiteSpace($protectedDestination)) {
        continue
    }
    $protectedFullPath = [IO.Path]::GetFullPath($protectedDestination)
    if ([string]::Equals(
        $InstallDir.TrimEnd('\'),
        $protectedFullPath.TrimEnd('\'),
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw "Refusing to replace protected system directory $InstallDir."
    }
}

$installParent = Split-Path -Parent $InstallDir
if ([string]::IsNullOrWhiteSpace($installParent)) {
    throw "Invalid installation directory: $InstallDir"
}

if (-not [string]::IsNullOrWhiteSpace($ArchivePath)) {
    if ([string]::IsNullOrWhiteSpace($Version)) {
        throw '-ArchivePath requires -Version (or A3S_BOX_VERSION).'
    }
    if ([string]::IsNullOrWhiteSpace($Sha256)) {
        throw '-ArchivePath requires -Sha256 (or A3S_BOX_SHA256).'
    }
    $tag = Get-NormalizedTag $Version
}

# Windows PowerShell 5.1 can otherwise negotiate TLS 1.0 on older hosts.
[Net.ServicePointManager]::SecurityProtocol = (
    [Net.ServicePointManager]::SecurityProtocol -bor
    [Net.SecurityProtocolType]::Tls12
)

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) (
    'a3s-box-install-{0}' -f [Guid]::NewGuid().ToString('N')
)
New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null

try {
    if ([string]::IsNullOrWhiteSpace($ArchivePath)) {
        if ([string]::IsNullOrWhiteSpace($Version)) {
            $apiUri = "$apiRoot/releases/latest"
            Write-InstallerMessage 'resolving the latest stable release'
        }
        else {
            $tag = Get-NormalizedTag $Version
            $escapedTag = [Uri]::EscapeDataString($tag)
            $apiUri = "$apiRoot/releases/tags/$escapedTag"
            Write-InstallerMessage "resolving $tag"
        }

        $release = Invoke-RestMethod -UseBasicParsing -Uri $apiUri `
            -Headers (Get-GitHubHeaders)
        $resolvedTag = [string]$release.tag_name
        if ([string]::IsNullOrWhiteSpace($resolvedTag)) {
            throw 'GitHub release metadata has no tag_name.'
        }
        if (
            -not [string]::IsNullOrWhiteSpace($Version) -and
            -not [string]::Equals($tag, $resolvedTag, [StringComparison]::Ordinal)
        ) {
            throw "GitHub returned release $resolvedTag when $tag was requested."
        }
        $tag = Get-NormalizedTag $resolvedTag
        $assetName = "a3s-box-$tag-$platform.zip"
        $assets = @($release.assets | Where-Object {
            [string]::Equals($_.name, $assetName, [StringComparison]::Ordinal)
        })
        if ($assets.Count -ne 1) {
            throw "Release $tag does not contain exactly one asset named $assetName."
        }

        $digestProperty = $assets[0].PSObject.Properties['digest']
        $publishedDigest = if ($null -ne $digestProperty) {
            [string]$digestProperty.Value
        }
        else {
            ''
        }
        if ($publishedDigest.StartsWith('sha256:', [StringComparison]::OrdinalIgnoreCase)) {
            $Sha256 = $publishedDigest.Substring(7)
        }
        elseif ([string]::IsNullOrWhiteSpace($publishedDigest)) {
            $manifest = Join-Path $temporaryDirectory "$assetName.sha256"
            try {
                Invoke-ReleaseDownload `
                    -Uri "$releaseRoot/$tag/$assetName.sha256" `
                    -Destination $manifest
                $Sha256 = Get-SidecarDigest -Manifest $manifest -AssetName $assetName
            }
            catch {
                $Sha256 = $null
            }
            if ([string]::IsNullOrWhiteSpace($Sha256)) {
                throw "Release $tag does not publish a SHA-256 digest for $assetName."
            }
        }
        else {
            throw "Release $tag publishes an unsupported digest: $publishedDigest"
        }

        $ArchivePath = Join-Path $temporaryDirectory $assetName
        Write-InstallerMessage "downloading $assetName"
        Invoke-ReleaseDownload `
            -Uri "$releaseRoot/$tag/$assetName" `
            -Destination $ArchivePath
    }
    else {
        if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
            throw "Archive does not exist: $ArchivePath"
        }
        $ArchivePath = (Resolve-Path -LiteralPath $ArchivePath).Path
        $assetName = "a3s-box-$tag-$platform.zip"
    }

    Assert-Sha256 $Sha256
    $Sha256 = $Sha256.ToLowerInvariant()
    $actualSha256 = (
        Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    if (-not [string]::Equals(
        $actualSha256,
        $Sha256,
        [StringComparison]::Ordinal
    )) {
        throw (
            "SHA-256 mismatch for {0}: expected {1}, found {2}" -f
            (Split-Path -Leaf $ArchivePath), $Sha256, $actualSha256
        )
    }
    Write-InstallerMessage "verified SHA-256 $actualSha256"

    $versionNumber = $tag.Substring(1)
    $packageDirectory = "a3s-box-$tag-$platform"
    Assert-ArchiveLayout -Archive $ArchivePath -ExpectedRoot $packageDirectory
    $extractRoot = Join-Path $temporaryDirectory 'extracted'
    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $extractRoot
    $packageRoot = Join-Path $extractRoot $packageDirectory
    if (-not (Test-Path -LiteralPath $packageRoot -PathType Container)) {
        throw "Archive is missing its $packageDirectory root."
    }

    $requiredFiles = @(
        'a3s-box.exe',
        'a3s-box-shim.exe',
        'a3s-box-guest-init',
        'krun.dll',
        'libkrunfw.dll'
    )
    foreach ($required in $requiredFiles) {
        $requiredPath = Join-Path $packageRoot $required
        if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
            throw "Archive is missing required file: $required"
        }
    }
    Assert-InstalledVersion `
        -Executable (Join-Path $packageRoot 'a3s-box.exe') `
        -ExpectedVersion $versionNumber

    if (Test-Path -LiteralPath $InstallDir) {
        $existingItem = Get-Item -LiteralPath $InstallDir -Force
        if (-not $existingItem.PSIsContainer) {
            throw "Installation path exists and is not a directory: $InstallDir"
        }
        if (($existingItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Installation path must not be a reparse point: $InstallDir"
        }
        $marker = Join-Path $InstallDir $markerName
        $existingEntries = @(Get-ChildItem -LiteralPath $InstallDir -Force)
        if (
            -not (Test-Path -LiteralPath $marker -PathType Leaf) -and
            $existingEntries.Count -gt 0 -and
            -not $Force
        ) {
            throw (
                "$InstallDir is not managed by this installer; " +
                'pass -Force to replace it.'
            )
        }
    }

    New-Item -ItemType Directory -Path $installParent -Force | Out-Null
    $stageDirectory = Join-Path $installParent (
        '.a3s-box-stage-{0}' -f [Guid]::NewGuid().ToString('N')
    )
    New-Item -ItemType Directory -Path $stageDirectory | Out-Null
    Get-ChildItem -LiteralPath $packageRoot -Force |
        Copy-Item -Destination $stageDirectory -Recurse -Force

    $markerContents = [ordered]@{
        schema   = 1
        version  = $tag
        platform = $platform
        sha256   = $actualSha256
    } | ConvertTo-Json
    Set-Content -LiteralPath (Join-Path $stageDirectory $markerName) `
        -Value $markerContents -Encoding UTF8
    Assert-InstalledVersion `
        -Executable (Join-Path $stageDirectory 'a3s-box.exe') `
        -ExpectedVersion $versionNumber

    $backupContainer = Join-Path $installParent (
        '.a3s-box-backup-{0}' -f [Guid]::NewGuid().ToString('N')
    )
    New-Item -ItemType Directory -Path $backupContainer | Out-Null
    $backupPath = Join-Path $backupContainer 'previous'
    $previousMoved = $false
    $newActivated = $false
    try {
        if (Test-Path -LiteralPath $InstallDir) {
            Move-Item -LiteralPath $InstallDir -Destination $backupPath
            $previousMoved = $true
        }
        Move-Item -LiteralPath $stageDirectory -Destination $InstallDir
        $stageDirectory = $null
        $newActivated = $true
        Assert-InstalledVersion `
            -Executable (Join-Path $InstallDir 'a3s-box.exe') `
            -ExpectedVersion $versionNumber
    }
    catch {
        $activationException = $_
        $rollbackFailures = @()
        if ($newActivated -and (Test-Path -LiteralPath $InstallDir)) {
            try {
                Remove-Item -LiteralPath $InstallDir -Recurse -Force
            }
            catch {
                $rollbackFailures += "could not remove the failed installation: $($_.Exception.Message)"
            }
        }
        if ($previousMoved) {
            try {
                if (-not (Test-Path -LiteralPath $backupPath)) {
                    throw "Previous installation backup is missing: $backupPath"
                }
                Move-Item -LiteralPath $backupPath -Destination $InstallDir
            }
            catch {
                $rollbackFailures += "could not restore the previous installation: $($_.Exception.Message)"
            }
        }
        if ($rollbackFailures.Count -gt 0) {
            $preservedBackup = $backupContainer
            $backupContainer = $null
            throw (
                "Activation failed: $($activationException.Exception.Message) " +
                "Rollback also failed: $($rollbackFailures -join '; '). " +
                "Recovery files were preserved at $preservedBackup."
            )
        }
        throw $activationException
    }

    Remove-Item -LiteralPath $backupContainer -Recurse -Force
    $backupContainer = $null

    if (-not $NoModifyPath) {
        try {
            Add-InstallDirectoryToUserPath -Directory $InstallDir
        }
        catch {
            Write-InstallerWarning (
                "installation succeeded, but PATH could not be updated: $($_.Exception.Message)"
            )
        }
    }

    Write-InstallerMessage "installed A3S Box $versionNumber for $platform at $InstallDir"
    if ($NoModifyPath) {
        Write-InstallerMessage "add $InstallDir to PATH before invoking a3s-box"
    }
    else {
        Write-InstallerMessage 'open a new terminal if this shell does not see the updated PATH'
    }
    Write-InstallerMessage 'verify with: a3s-box --version'
}
finally {
    if (
        -not [string]::IsNullOrWhiteSpace($stageDirectory) -and
        (Test-Path -LiteralPath $stageDirectory)
    ) {
        Remove-Item -LiteralPath $stageDirectory -Recurse -Force
    }
    if (
        -not [string]::IsNullOrWhiteSpace($backupContainer) -and
        (Test-Path -LiteralPath $backupContainer)
    ) {
        Remove-Item -LiteralPath $backupContainer -Recurse -Force
    }
    if (
        -not [string]::IsNullOrWhiteSpace($temporaryDirectory) -and
        (Test-Path -LiteralPath $temporaryDirectory)
    ) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
