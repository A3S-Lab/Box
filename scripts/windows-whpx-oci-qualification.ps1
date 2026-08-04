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
    [string]$OutputDirectory = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'The Box/WHPX OCI qualification must run on Windows.'
}

$expectedRootfsSha256 = '4b4daa9fe2fc696c4919c4412a4c3d3e770d8fb70292a004a2c72f5096175282'
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$boxArtifacts = (Resolve-Path -LiteralPath $BoxArtifactDirectory -ErrorAction Stop).Path
$ociWindowsArtifacts = (
    Resolve-Path -LiteralPath $OciWindowsArtifactDirectory -ErrorAction Stop
).Path
$ociGuestArtifacts = (
    Resolve-Path -LiteralPath $OciGuestArtifactDirectory -ErrorAction Stop
).Path
$rootfsArchive = (Resolve-Path -LiteralPath $RootfsArchive -ErrorAction Stop).Path
$tar = (Get-Command tar.exe -ErrorAction Stop).Source
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

$runId = '{0}-{1}' -f (
    Get-Date
).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'), $PID
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot `
        "target\windows-whpx-oci-qualification\$runId"
}
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $outputRoot) {
    throw "Refusing to reuse a qualification directory: $outputRoot"
}

$boxHome = Join-Path $outputRoot 'box-whpx-oci-qualification-home'
$runtimeRoot = Join-Path $boxHome 'oci-runtime'
$systemRoot = Join-Path $runtimeRoot 'system'
$stateRoot = Join-Path $runtimeRoot 'state'
$readyPath = Join-Path $runtimeRoot 'service-ready.json'
$boxBin = Join-Path $outputRoot 'box-bin'
$ociBin = Join-Path $outputRoot 'oci-bin'
$reportPath = Join-Path $outputRoot 'report.json'
$summaryPath = Join-Path $outputRoot 'summary.json'
$serviceStdoutPath = Join-Path $outputRoot 'oci-service.stdout.log'
$serviceStderrPath = Join-Path $outputRoot 'oci-service.stderr.log'
$importStdoutPath = Join-Path $outputRoot 'box-import.stdout.log'
$importStderrPath = Join-Path $outputRoot 'box-import.stderr.log'
$qualificationStdoutPath = Join-Path $outputRoot 'qualification.stdout.log'
$qualificationStderrPath = Join-Path $outputRoot 'qualification.stderr.log'
$pipeName = '\\.\pipe\a3s-oci-box-qualification-{0}-{1}' -f $PID, (
    [Guid]::NewGuid().ToString('N')
)
$image = 'a3s-box-whpx-oci-qualification:local'
$startedAt = [DateTime]::UtcNow

function Write-Utf8Text {
    param(
        [Parameter(Mandatory)]
        [string]$Path,
        [AllowEmptyString()]
        [Parameter(Mandatory)]
        [string]$Text
    )

    [IO.File]::WriteAllText($Path, $Text, $script:utf8NoBom)
}

function Write-JsonFile {
    param(
        [Parameter(Mandatory)]
        [string]$Path,
        [Parameter(Mandatory)]
        [object]$Value
    )

    Write-Utf8Text -Path $Path -Text (
        ConvertTo-Json -InputObject $Value -Depth 30
    )
}

function Resolve-RegularFile {
    param([Parameter(Mandatory)][string]$Path)

    $resolved = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path
    $item = Get-Item -LiteralPath $resolved -Force
    if ($item.PSIsContainer -or $item.Length -le 0) {
        throw "Expected a non-empty regular file: $resolved"
    }
    if ($item.PSObject.Properties.Name -contains 'LinkType' -and $item.LinkType) {
        throw "Qualification inputs must not be links: $resolved"
    }
    $resolved
}

function Read-VerifiedArtifactManifest {
    param(
        [Parameter(Mandatory)][string]$Directory,
        [Parameter(Mandatory)][string]$Schema,
        [Parameter(Mandatory)][string]$ExpectedSourceCommit
    )

    $manifestPath = Resolve-RegularFile (
        Join-Path $Directory 'artifact-manifest.json'
    )
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.schema_version -ne $Schema) {
        throw "Unexpected artifact schema in ${manifestPath}: $($manifest.schema_version)"
    }
    if ($manifest.source_commit -ne $ExpectedSourceCommit) {
        throw "Artifact source commit mismatch in ${manifestPath}: expected $ExpectedSourceCommit, found $($manifest.source_commit)"
    }
    if ($manifest.workflow_commit -notmatch '^[0-9a-f]{40}$') {
        throw "Artifact workflow commit is invalid in $manifestPath"
    }
    foreach ($record in @($manifest.files)) {
        $name = [string]$record.name
        if ([string]::IsNullOrWhiteSpace($name) -or
            [IO.Path]::GetFileName($name) -cne $name) {
            throw "Artifact manifest contains an unsafe file name: $name"
        }
        $path = Resolve-RegularFile (Join-Path $Directory $name)
        $item = Get-Item -LiteralPath $path
        $sha256 = (
            Get-FileHash -LiteralPath $path -Algorithm SHA256
        ).Hash.ToLowerInvariant()
        if ($item.Length -ne [long]($record.size) -or $sha256 -ne $record.sha256) {
            throw "Artifact file does not match its manifest: $path"
        }
    }
    $manifest
}

function Require-ManifestFile {
    param(
        [Parameter(Mandatory)][object]$Manifest,
        [Parameter(Mandatory)][string]$Name
    )

    $matches = @($Manifest.files | Where-Object { $_.name -eq $Name })
    if ($matches.Count -ne 1) {
        throw "Artifact manifest must contain exactly one $Name entry"
    }
}

function Join-QuotedNativeArguments {
    param([Parameter(Mandatory)][string[]]$Arguments)

    @($Arguments | ForEach-Object {
        if ($_.Contains('"')) {
            throw "Native argument contains an unsupported quote: $_"
        }
        '"{0}"' -f $_
    }) -join ' '
}

function Get-A3sProcesses {
    @(
        Get-Process -ErrorAction SilentlyContinue |
            Where-Object {
                $_.ProcessName -in @(
                    'a3s-box',
                    'a3s-box-shim',
                    'a3s-oci',
                    'a3s-oci-krun-shim',
                    'windows-whpx-oci-qualification'
                )
            } |
            Select-Object ProcessName, Id, StartTime, Path
    )
}

function Wait-ForNoA3sProcesses {
    for ($attempt = 0; $attempt -lt 150; $attempt++) {
        $processes = @(Get-A3sProcesses)
        if ($processes.Count -eq 0) {
            return @()
        }
        Start-Sleep -Milliseconds 100
    }
    @(Get-A3sProcesses)
}

function Directory-IsAbsentOrEmpty {
    param([Parameter(Mandatory)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        return $true
    }
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return $false
    }
    $null -eq (Get-ChildItem -LiteralPath $Path -Force | Select-Object -First 1)
}

$commit = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -notmatch '^[0-9a-f]{40}$') {
    throw 'Unable to resolve the Box source commit.'
}
$worktreeStatus = @(& git -C $repositoryRoot status --porcelain)
if ($LASTEXITCODE -ne 0) {
    throw 'Unable to inspect the Box worktree.'
}
$ciWorkflow = Get-Content -LiteralPath (
    Join-Path $repositoryRoot '.github\workflows\ci.yml'
) -Raw
$pinnedMatch = [regex]::Match(
    $ciWorkflow,
    '(?m)^\s*A3S_OCI_RUNTIME_REV:\s*([0-9a-f]{40})\s*$'
)
if (-not $pinnedMatch.Success) {
    throw 'Unable to resolve the exact OCI Runtime revision from CI.'
}
$pinnedOciCommit = $pinnedMatch.Groups[1].Value

$boxManifest = Read-VerifiedArtifactManifest `
    -Directory $boxArtifacts `
    -Schema 'a3s.box.windows-whpx-qualification-artifact.v1' `
    -ExpectedSourceCommit $commit
$ociWindowsManifest = Read-VerifiedArtifactManifest `
    -Directory $ociWindowsArtifacts `
    -Schema 'a3s.oci.windows-whpx-qualification-artifact.v1' `
    -ExpectedSourceCommit $pinnedOciCommit
$ociGuestManifest = Read-VerifiedArtifactManifest `
    -Directory $ociGuestArtifacts `
    -Schema 'a3s.oci.guest-agent-qualification-artifact.v1' `
    -ExpectedSourceCommit $pinnedOciCommit

foreach ($name in @(
    'a3s-box.exe',
    'a3s-box-shim.exe',
    'windows-whpx-oci-qualification.exe',
    'krun.dll',
    'libkrunfw.dll'
)) {
    Require-ManifestFile -Manifest $boxManifest -Name $name
}
foreach ($name in @(
    'a3s-oci.exe',
    'a3s-oci-krun-shim.exe',
    'krun.dll',
    'libkrunfw.dll'
)) {
    Require-ManifestFile -Manifest $ociWindowsManifest -Name $name
}
Require-ManifestFile -Manifest $ociGuestManifest -Name 'a3s-oci-agent-x86_64'
if ($ociWindowsManifest.run_id -ne $ociGuestManifest.run_id -or
    $ociWindowsManifest.workflow_commit -ne $ociGuestManifest.workflow_commit) {
    throw 'OCI Windows and guest artifacts must come from the same workflow run.'
}

$rootfsArchive = Resolve-RegularFile $rootfsArchive
$rootfsSha256 = (
    Get-FileHash -LiteralPath $rootfsArchive -Algorithm SHA256
).Hash.ToLowerInvariant()
if ($rootfsSha256 -ne $expectedRootfsSha256) {
    throw "Rootfs SHA-256 mismatch: expected $expectedRootfsSha256, found $rootfsSha256"
}

$preexisting = @(Get-A3sProcesses)
if ($preexisting.Count -gt 0) {
    $description = ($preexisting | ForEach-Object {
        '{0}:{1}' -f $_.ProcessName, $_.Id
    }) -join ', '
    throw "Refusing to start with active A3S runtime processes: $description"
}

New-Item -ItemType Directory -Path `
    $outputRoot, $boxHome, $systemRoot, $stateRoot, $boxBin, $ociBin | Out-Null
foreach ($name in @(
    'a3s-box.exe',
    'a3s-box-shim.exe',
    'windows-whpx-oci-qualification.exe',
    'krun.dll',
    'libkrunfw.dll'
)) {
    Copy-Item -LiteralPath (Join-Path $boxArtifacts $name) `
        -Destination (Join-Path $boxBin $name)
}
foreach ($name in @(
    'a3s-oci.exe',
    'a3s-oci-krun-shim.exe',
    'krun.dll',
    'libkrunfw.dll'
)) {
    Copy-Item -LiteralPath (Join-Path $ociWindowsArtifacts $name) `
        -Destination (Join-Path $ociBin $name)
}

& $tar -xf $rootfsArchive -C $systemRoot
if ($LASTEXITCODE -ne 0) {
    throw 'Failed to extract the WHPX utility-VM system root.'
}
New-Item -ItemType Directory -Path `
    (Join-Path $systemRoot 'run\a3s-oci-runtime'), `
    (Join-Path $systemRoot 'usr\bin') -Force | Out-Null
Copy-Item -LiteralPath (
    Join-Path $ociGuestArtifacts 'a3s-oci-agent-x86_64'
) -Destination (Join-Path $systemRoot 'usr\bin\a3s-oci-agent')

$boxCli = Join-Path $boxBin 'a3s-box.exe'
$qualification = Join-Path $boxBin 'windows-whpx-oci-qualification.exe'
$ociCli = Join-Path $ociBin 'a3s-oci.exe'
$ociShim = Join-Path $ociBin 'a3s-oci-krun-shim.exe'
$serviceArguments = @(
    'box-whpx-qualification-service',
    '--shim', $ociShim,
    '--runtime-root', $runtimeRoot,
    '--vm-rootfs', $systemRoot,
    '--state-root', $stateRoot,
    '--pipe', $pipeName,
    '--ready-file', $readyPath
)

$service = $null
$ready = $null
$qualificationReport = $null
$failure = $null
$result = 'failed'
$residual = @()
$previousEnvironment = [ordered]@{}
foreach ($name in @(
    'A3S_HOME',
    'A3S_BOX_WHPX_OCI_QUALIFICATION',
    'A3S_BOX_OCI_HOST_ROOT',
    'A3S_BOX_OCI_WHPX_ENDPOINT',
    'A3S_BOX_WHPX_OCI_IMAGE',
    'A3S_BOX_WHPX_OCI_REPORT'
)) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable(
        $name,
        [EnvironmentVariableTarget]::Process
    )
}

try {
    $service = Start-Process -FilePath $ociCli `
        -ArgumentList (Join-QuotedNativeArguments -Arguments $serviceArguments) `
        -WorkingDirectory $ociBin `
        -RedirectStandardOutput $serviceStdoutPath `
        -RedirectStandardError $serviceStderrPath `
        -WindowStyle Hidden -PassThru

    for ($attempt = 0; $attempt -lt 300; $attempt++) {
        if ($service.HasExited) {
            throw "OCI qualification service exited with code $($service.ExitCode) before readiness"
        }
        if (Test-Path -LiteralPath $readyPath -PathType Leaf) {
            $candidate = Get-Content -LiteralPath $readyPath -Raw | ConvertFrom-Json
            if ($candidate.schema_version -eq 'a3s.oci.box-whpx-service-ready.v1') {
                $ready = $candidate
                break
            }
        }
        Start-Sleep -Milliseconds 100
    }
    if ($null -eq $ready) {
        throw 'Timed out waiting for the OCI qualification service.'
    }
    if ($ready.owner_pid -ne $service.Id -or
        $ready.endpoint -ne $pipeName -or
        [IO.Path]::GetFullPath($ready.runtime_root) -ne $runtimeRoot -or
        [IO.Path]::GetFullPath($ready.state_root) -ne $stateRoot) {
        throw 'OCI qualification readiness evidence does not match the launched owner.'
    }

    $env:A3S_HOME = $boxHome
    $importProcess = Start-Process -FilePath $boxCli `
        -ArgumentList (Join-QuotedNativeArguments -Arguments @(
            'import', $rootfsArchive, $image
        )) `
        -WorkingDirectory $boxBin `
        -RedirectStandardOutput $importStdoutPath `
        -RedirectStandardError $importStderrPath `
        -WindowStyle Hidden -Wait -PassThru
    if ($importProcess.ExitCode -ne 0) {
        throw "Box image import failed with exit code $($importProcess.ExitCode)"
    }

    $env:A3S_BOX_WHPX_OCI_QUALIFICATION = '1'
    $env:A3S_BOX_OCI_HOST_ROOT = $runtimeRoot
    $env:A3S_BOX_OCI_WHPX_ENDPOINT = $pipeName
    $env:A3S_BOX_WHPX_OCI_IMAGE = $image
    $env:A3S_BOX_WHPX_OCI_REPORT = $reportPath
    $qualificationProcess = Start-Process -FilePath $qualification `
        -WorkingDirectory $boxBin `
        -RedirectStandardOutput $qualificationStdoutPath `
        -RedirectStandardError $qualificationStderrPath `
        -WindowStyle Hidden -Wait -PassThru
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
        throw 'Box qualification executable did not emit its report.'
    }
    $qualificationReport = Get-Content -LiteralPath $reportPath -Raw |
        ConvertFrom-Json
    if ($qualificationProcess.ExitCode -ne 0 -or
        $qualificationReport.schema_version -ne 'a3s.box.windows-whpx-oci-qualification.v1' -or
        $qualificationReport.status -ne 'passed') {
        throw "Box qualification failed with exit code $($qualificationProcess.ExitCode)"
    }
    if (-not $qualificationReport.create_replay_exact -or
        -not $qualificationReport.manager_restart_reconciled -or
        -not $qualificationReport.observed_running -or
        $qualificationReport.terminal_state -ne 'stopped' -or
        $qualificationReport.exit_code -ne 23 -or
        $qualificationReport.runtime_binding.driver -ne 'libkrun-whpx' -or
        $qualificationReport.runtime_binding.isolation -ne 'dedicated-vm' -or
        -not $qualificationReport.removed -or
        -not $qualificationReport.remove_replay_absent -or
        -not $qualificationReport.reconcile_absent -or
        -not $qualificationReport.box_directory_absent -or
        -not $qualificationReport.runtime_shares_absent -or
        -not $qualificationReport.bundle_handoffs_absent) {
        throw 'Box qualification report does not satisfy the complete lifecycle contract.'
    }
    if (-not (Directory-IsAbsentOrEmpty (Join-Path $runtimeRoot 'shares')) -or
        -not (Directory-IsAbsentOrEmpty (Join-Path $runtimeRoot 'bundle-handoffs'))) {
        throw 'OCI runtime shares or Box bundle handoffs remain after qualification.'
    }
    $result = 'passed'
}
catch {
    $failure = $_.Exception.Message
}
finally {
    foreach ($name in $previousEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable(
            $name,
            $previousEnvironment[$name],
            [EnvironmentVariableTarget]::Process
        )
    }
    if ($null -ne $service -and -not $service.HasExited) {
        Stop-Process -Id $service.Id -Force -ErrorAction SilentlyContinue
        $service.WaitForExit(15000) | Out-Null
    }
    $residual = @(Wait-ForNoA3sProcesses)
    if ($residual.Count -gt 0) {
        foreach ($process in $residual) {
            if ($null -ne $process.Path -and
                [IO.Path]::GetFullPath($process.Path).StartsWith(
                    $outputRoot,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            }
        }
        if ($null -eq $failure) {
            $failure = 'Qualification leaked one or more staged A3S processes.'
        }
        $result = 'failed'
    }

    $summary = [ordered]@{
        schema_version = 'a3s.box.windows-whpx-oci-qualification-run.v1'
        status = $result
        started_at_utc = $startedAt.ToString('o')
        completed_at_utc = [DateTime]::UtcNow.ToString('o')
        error = $failure
        box_commit = $commit
        box_worktree_status = $worktreeStatus
        pinned_oci_commit = $pinnedOciCommit
        rootfs_archive = $rootfsArchive
        rootfs_sha256 = $rootfsSha256
        endpoint = $pipeName
        box_artifact = $boxManifest
        oci_windows_artifact = $ociWindowsManifest
        oci_guest_artifact = $ociGuestManifest
        service_ready = $ready
        qualification = $qualificationReport
        residual_processes = $residual
    }
    Write-JsonFile -Path $summaryPath -Value $summary
}

if ($null -ne $failure) {
    throw "$failure; see $summaryPath"
}
Write-Output "Box/WHPX OCI qualification passed: $summaryPath"
Get-Content -LiteralPath $summaryPath
