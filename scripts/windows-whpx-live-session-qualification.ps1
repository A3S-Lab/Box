# Local Windows/WHPX MicroVM mid-run Live observation gate for Box.
#
# Box-owned Host only (packaged production analogue). Mirrors the BoxOwned
# layout of windows-whpx-oci-qualification.ps1. Does not tip-prove binder gate 9
# until a retained Live report passes verify-windows-whpx-live-session-report.py.
# Keeps b2_process_session_recovery_closed=false. Does not claim Enterprise GA.

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$BoxArtifactDirectory,
    [Parameter(Mandatory)]
    [string]$OciWindowsArtifactDirectory,
    [Parameter(Mandatory)]
    [string]$OciGuestArtifactDirectory,
    [Parameter(Mandatory)]
    [string]$RootfsArchive,
    [string]$OutputDirectory = '',
    [string]$Image = 'a3s-box-whpx-live-session:local'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'windows-whpx-live-session-qualification requires Windows.'
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$boxArtifacts = (Resolve-Path -LiteralPath $BoxArtifactDirectory -ErrorAction Stop).Path
$ociWindowsArtifacts = (
    Resolve-Path -LiteralPath $OciWindowsArtifactDirectory -ErrorAction Stop
).Path
$ociGuestArtifacts = (
    Resolve-Path -LiteralPath $OciGuestArtifactDirectory -ErrorAction Stop
).Path
$rootfsArchive = (Resolve-Path -LiteralPath $RootfsArchive -ErrorAction Stop).Path
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

$runId = '{0}-{1}' -f (
    Get-Date
).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'), $PID
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot `
        "target\windows-whpx-live-session-qualification\$runId"
}
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $outputRoot) {
    throw "Refusing to reuse a qualification directory: $outputRoot"
}

$boxHome = Join-Path $outputRoot 'box-whpx-live-session-home'
$runtimeRoot = Join-Path $boxHome 'oci-runtime'
$systemRoot = Join-Path $runtimeRoot 'system'
$boxBin = Join-Path $outputRoot 'box-bin'
$ociBin = Join-Path $outputRoot 'oci-bin'
$reportPath = Join-Path $outputRoot 'report.json'
$importStdoutPath = Join-Path $outputRoot 'box-import.stdout.log'
$importStderrPath = Join-Path $outputRoot 'box-import.stderr.log'
$qualificationStdoutPath = Join-Path $outputRoot 'qualification.stdout.log'
$qualificationStderrPath = Join-Path $outputRoot 'qualification.stderr.log'
$exampleName = 'windows-whpx-live-session-qualification.exe'

New-Item -ItemType Directory -Path @(
    $boxHome, $runtimeRoot, $systemRoot, $boxBin, $ociBin
) | Out-Null

function Resolve-RegularFile {
    param([Parameter(Mandatory)][string]$Path)
    $resolved = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path
    $item = Get-Item -LiteralPath $resolved -Force
    if ($item.PSIsContainer -or $item.Length -le 0) {
        throw "Expected a non-empty regular file: $resolved"
    }
    $resolved
}

Copy-Item -LiteralPath (Resolve-RegularFile (Join-Path $boxArtifacts 'a3s-box.exe')) `
    -Destination (Join-Path $boxBin 'a3s-box.exe')
$exampleSrc = Join-Path $boxArtifacts $exampleName
if (-not (Test-Path -LiteralPath $exampleSrc)) {
    throw @"
Missing $exampleName in $BoxArtifactDirectory.
Build with:
  cargo build -p a3s-box-runtime --example windows-whpx-live-session-qualification --release --features vm
"@
}
Copy-Item -LiteralPath (Resolve-RegularFile $exampleSrc) `
    -Destination (Join-Path $boxBin $exampleName)

foreach ($name in @('a3s-oci.exe', 'a3s-oci-krun-shim.exe', 'krun.dll', 'libkrunfw.dll')) {
    $candidate = Join-Path $ociWindowsArtifacts $name
    if (-not (Test-Path -LiteralPath $candidate)) {
        $candidate = Join-Path $ociWindowsArtifacts (Join-Path 'bin' $name)
    }
    Copy-Item -LiteralPath (Resolve-RegularFile $candidate) `
        -Destination (Join-Path $ociBin $name)
}

# Immutable system-image must stay disjoint from the mutable Host runtime root.
$systemImageSrc = Join-Path $ociWindowsArtifacts 'system-image'
if (-not (Test-Path -LiteralPath $systemImageSrc -PathType Container)) {
    throw "Missing system-image directory under $OciWindowsArtifactDirectory"
}
$systemImageRoot = Join-Path $outputRoot 'system-image'
Copy-Item -LiteralPath $systemImageSrc `
    -Destination $systemImageRoot -Recurse
$systemImageManifest = Resolve-RegularFile (
    Join-Path $systemImageRoot 'system-image.json'
)

# Guest agent bits may live beside the Windows Host package; keep path for operators.
$null = $ociGuestArtifacts

$boxSha = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $boxSha -notmatch '^[0-9a-f]{40}$') {
    throw 'Unable to resolve the Box source commit.'
}
$ciWorkflow = Get-Content -LiteralPath (
    Join-Path $repositoryRoot '.github\workflows\ci.yml'
) -Raw
$pinnedMatch = [regex]::Match(
    $ciWorkflow,
    '(?m)^\s*A3S_OCI_RUNTIME_REV:\s*([0-9a-f]{40})\s*$'
)
if (-not $pinnedMatch.Success) {
    throw 'Unable to resolve A3S_OCI_RUNTIME_REV from CI.'
}
$ociSha = $pinnedMatch.Groups[1].Value

$hasher = [System.Security.Cryptography.SHA256]::Create()
$bytes = [Text.Encoding]::UTF8.GetBytes($runtimeRoot)
$digest = ($hasher.ComputeHash($bytes) |
    ForEach-Object { $_.ToString('x2') }) -join ''
$pipeName = '\\.\pipe\a3s-box-whpx-owner-{0}' -f $digest.Substring(0, 32)

$env:A3S_HOME = $boxHome
$env:PATH = "$boxBin;$ociBin;$env:PATH"

Write-Host "importing rootfs archive into image $Image"
$importProcess = Start-Process -FilePath (Join-Path $boxBin 'a3s-box.exe') `
    -ArgumentList @('import', $rootfsArchive, $Image) `
    -WorkingDirectory $boxBin `
    -RedirectStandardOutput $importStdoutPath `
    -RedirectStandardError $importStderrPath `
    -WindowStyle Hidden -Wait -PassThru
if ($importProcess.ExitCode -ne 0) {
    throw "Box image import failed with exit code $($importProcess.ExitCode)"
}

$env:A3S_BOX_WHPX_LIVE_SESSION_QUALIFICATION = '1'
$env:A3S_BOX_WHPX_OCI_BOX_OWNED = '1'
$env:A3S_BOX_OCI_HOST_ROOT = $runtimeRoot
$env:A3S_BOX_OCI_WHPX_ENDPOINT = $pipeName
$env:A3S_BOX_OCI_MIGRATION = 'microvm'
$env:A3S_BOX_WHPX_LIVE_SESSION_IMAGE = $Image
$env:A3S_BOX_WHPX_LIVE_SESSION_REPORT = $reportPath
$env:A3S_BOX_WHPX_LIVE_SESSION_BOX_SHA = $boxSha
$env:A3S_BOX_WHPX_LIVE_SESSION_OCI_SHA = $ociSha
# Service paths reuse the WHPX OCI qualification env names (see example).
$env:A3S_BOX_WHPX_OCI_SERVICE_BIN = (Join-Path $ociBin 'a3s-oci.exe')
$env:A3S_BOX_WHPX_OCI_SERVICE_ROOT = $runtimeRoot
$env:A3S_BOX_WHPX_OCI_SERVICE_SHIM = (Join-Path $ociBin 'a3s-oci-krun-shim.exe')
$env:A3S_BOX_WHPX_OCI_SERVICE_VM_ROOTFS = $systemRoot
$env:A3S_BOX_WHPX_OCI_SERVICE_MANIFEST = $systemImageManifest
$env:A3S_BOX_WHPX_OCI_SERVICE_LOG = (
    Join-Path $outputRoot 'qualification-service.log'
)

Write-Host 'running Windows/WHPX mid-run Live observation harness (Box-owned)'
Write-Host "  home=$boxHome"
Write-Host "  service-root=$runtimeRoot"
Write-Host "  image=$Image"
Write-Host "  box-sha=$boxSha"
Write-Host "  oci-sha=$ociSha"
Write-Host "  report=$reportPath"
Write-Host '  session-owner=A3S_OCI_WHPX_SESSION_OWNER=1 (Box-owned Host spawn)'
Write-Host '  note=gate 9 remains open until retained Live verifies'

# Box-owned Host path sets this in oci_whpx_owner; export for any direct OCI
# create the example may issue outside that fence.
$env:A3S_OCI_WHPX_SESSION_OWNER = '1'

$qualification = Join-Path $boxBin $exampleName
$qualificationProcess = Start-Process -FilePath $qualification `
    -WorkingDirectory $boxBin `
    -RedirectStandardOutput $qualificationStdoutPath `
    -RedirectStandardError $qualificationStderrPath `
    -WindowStyle Hidden -Wait -PassThru
if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
    throw "qualification did not emit report; see $qualificationStderrPath"
}
if ($qualificationProcess.ExitCode -ne 0) {
    throw "WHPX live-session qualification failed with exit $($qualificationProcess.ExitCode); see $qualificationStderrPath"
}

$verifier = Join-Path $PSScriptRoot 'verify-windows-whpx-live-session-report.py'
$verifierOk = $false
foreach ($py in @('python', 'py')) {
    $cmd = Get-Command $py -ErrorAction SilentlyContinue
    if ($null -eq $cmd) { continue }
    if ($py -eq 'py') {
        & $cmd.Source -3 $verifier $reportPath
    } else {
        & $cmd.Source $verifier $reportPath
    }
    if ($LASTEXITCODE -eq 0) {
        $verifierOk = $true
        break
    }
}
if (-not $verifierOk) {
    Write-Warning "Report honesty verifier did not pass (or Python missing): $reportPath"
}

[IO.File]::WriteAllText(
    (Join-Path $outputRoot 'summary.txt'),
    "report=$reportPath`nexit=$($qualificationProcess.ExitCode)`n",
    $utf8NoBom
)
Write-Host "WHPX live-session observation finished: $reportPath"
