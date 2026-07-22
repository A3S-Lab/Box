[CmdletBinding()]
param(
    [string]$ImageTar = $env:A3S_BOX_TEST_ALPINE_TAR,
    [ValidateRange(0, 1000000)]
    [int]$Iterations = 1,
    [ValidateRange(0, 31536000)]
    [int]$DurationSeconds = 0,
    [ValidateRange(1, 86400)]
    [int]$CommandTimeoutSeconds = 300,
    [ValidateRange(1, 86400)]
    [int]$VirtiofsTimeoutSeconds = 900,
    [string]$OutputDirectory = '',
    [switch]$SkipBuild,
    [switch]$SkipVirtiofsStress,
    [switch]$ListTests
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$workspace = Join-Path $repositoryRoot 'src'
$tests = @(
    'real_core_lifecycle_pull_run_exec_logs_stop_rm',
    'real_core_create_start_preserves_command_override',
    'real_core_foreground_run_returns_exit_code_and_logs',
    'real_core_long_argument_avoids_kernel_cmdline_overflow',
    'real_core_utility_commands_cp_top_stats',
    'real_core_published_port_http_smoke',
    'real_core_bind_mounts_preserve_host_paths_and_read_only_mode',
    'real_core_named_volume_persists_across_stop_restart',
    'real_core_commit_preserves_guest_ownership_and_modes_after_stop',
    'real_core_filesystem_image_snapshot_commands',
    'real_core_virtiofs_tar_closes_every_source_file_cleanly'
)

if ($SkipVirtiofsStress) {
    $tests = @($tests | Where-Object {
        $_ -ne 'real_core_virtiofs_tar_closes_every_source_file_cleanly'
    })
}

if ($ListTests) {
    $tests
    return
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'The Windows WHPX soak runner must run on Windows.'
}
if ($Iterations -eq 0 -and $DurationSeconds -eq 0) {
    throw 'Specify a positive iteration count, duration, or both.'
}
if ([string]::IsNullOrWhiteSpace($ImageTar)) {
    throw 'Pass -ImageTar or set A3S_BOX_TEST_ALPINE_TAR.'
}

$resolvedImageTar = (Resolve-Path -LiteralPath $ImageTar -ErrorAction Stop).Path
$imageItem = Get-Item -LiteralPath $resolvedImageTar -Force
if (-not $imageItem.PSIsContainer -and $imageItem.Length -le 0) {
    throw "OCI image archive is empty: $resolvedImageTar"
}
if ($imageItem.PSIsContainer) {
    throw "OCI image archive is not a regular file: $resolvedImageTar"
}
if ($imageItem.PSObject.Properties.Name -contains 'LinkType' -and $imageItem.LinkType) {
    throw "OCI image archive must not be a link: $resolvedImageTar"
}
$imageTarSha256 = (Get-FileHash -LiteralPath $resolvedImageTar -Algorithm SHA256).Hash.ToLowerInvariant()

$runId = '{0}-{1}' -f (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'), $PID
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $workspace "target/a3s-box-whpx-soak/$runId"
}
$evidenceDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $evidenceDirectory -Force | Out-Null

function Get-BoxProcesses {
    @(
        Get-Process -Name 'a3s-box', 'a3s-box-shim' -ErrorAction SilentlyContinue |
            Select-Object Id, ProcessName, StartTime, CPU, Handles, WorkingSet64, Path
    )
}

function Wait-ForBoxProcessesToExit {
    param([int]$TimeoutSeconds = 10)

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $processes = @(Get-BoxProcesses)
        if ($processes.Count -eq 0) {
            return @()
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)

    @(Get-BoxProcesses)
}

function Invoke-LoggedNative {
    param(
        [Parameter(Mandatory)]
        [string]$Label,
        [Parameter(Mandatory)]
        [string]$LogPath,
        [Parameter(Mandatory)]
        [string]$FilePath,
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    Write-Host "+ $FilePath $($Arguments -join ' ')"
    $exitCode = -1
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        # Windows PowerShell wraps native stderr lines as non-terminating error
        # records. Cargo writes ordinary warnings there, so the native exit code
        # remains the authoritative result while output is captured.
        $ErrorActionPreference = 'Continue'
        & $FilePath @Arguments 2>&1 | Tee-Object -FilePath $LogPath
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($exitCode -ne 0) {
        throw "$Label failed with exit code $exitCode (log: $LogPath)"
    }
}

$commit = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) {
    throw 'Unable to resolve the repository commit.'
}
$worktreeStatus = @(& git -C $repositoryRoot status --porcelain)
if ($LASTEXITCODE -ne 0) {
    throw 'Unable to inspect the repository worktree.'
}

$startedAt = [DateTime]::UtcNow
$soakStartedAt = $null
$samples = @()
$completedTests = 0
$completedIterations = 0
$failure = $null
$result = 'running'

function Write-Summary {
    $finishedAt = [DateTime]::UtcNow
    $summary = [ordered]@{
        schema = 'a3s.box.windows-whpx-soak.v1'
        run_id = $runId
        result = $result
        commit = $commit
        worktree_dirty = $worktreeStatus.Count -gt 0
        image_tar = $resolvedImageTar
        image_tar_sha256 = $imageTarSha256
        started_at = $startedAt.ToString('o')
        soak_started_at = if ($null -eq $soakStartedAt) {
            $null
        }
        else {
            $soakStartedAt.ToString('o')
        }
        finished_at = $finishedAt.ToString('o')
        duration_seconds = [Math]::Round(($finishedAt - $startedAt).TotalSeconds, 3)
        requested_iterations = $Iterations
        requested_duration_seconds = $DurationSeconds
        completed_iterations = $completedIterations
        completed_tests = $completedTests
        command_timeout_seconds = $CommandTimeoutSeconds
        virtiofs_timeout_seconds = $VirtiofsTimeoutSeconds
        selected_tests = $tests
        failure = $failure
        samples = $samples
    }
    $summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (
        Join-Path $evidenceDirectory 'summary.json'
    ) -Encoding utf8
}

try {
    $preexisting = @(Get-BoxProcesses)
    if ($preexisting.Count -gt 0) {
        $description = ($preexisting | ForEach-Object {
            '{0}:{1}' -f $_.ProcessName, $_.Id
        }) -join ', '
        throw "Refusing to start with active A3S Box processes: $description"
    }

    Get-ComputerInfo -Property WindowsProductName, WindowsVersion, OsBuildNumber, OsArchitecture |
        ConvertTo-Json | Set-Content -LiteralPath (
            Join-Path $evidenceDirectory 'host.json'
        ) -Encoding utf8

    Set-Location -LiteralPath $workspace
    if (-not $SkipBuild) {
        Invoke-LoggedNative -Label 'guest-init build' `
            -LogPath (Join-Path $evidenceDirectory 'build-guest-init.log') `
            -FilePath 'cargo' `
            -Arguments @(
                'zigbuild', '--release', '--target', 'x86_64-unknown-linux-musl',
                '-p', 'a3s-box-guest-init'
            )
        Invoke-LoggedNative -Label 'Windows binary build' `
            -LogPath (Join-Path $evidenceDirectory 'build-windows.log') `
            -FilePath 'cargo' `
            -Arguments @('build', '-p', 'a3s-box-cli', '-p', 'a3s-box-shim')
    }

    $env:A3S_BOX_SMOKE_IMAGE_TAR = $resolvedImageTar
    $env:A3S_BOX_SMOKE_TIMEOUT_SECS = $CommandTimeoutSeconds.ToString()
    $env:A3S_BOX_VIRTIOFS_TAR_TIMEOUT_SECS = $VirtiofsTimeoutSeconds.ToString()
    $soakStartedAt = [DateTime]::UtcNow

    while ($true) {
        if ($Iterations -gt 0 -and $completedIterations -ge $Iterations) {
            break
        }
        if ($DurationSeconds -gt 0 -and
            ([DateTime]::UtcNow - $soakStartedAt).TotalSeconds -ge $DurationSeconds) {
            break
        }

        $iteration = $completedIterations + 1
        foreach ($test in $tests) {
            $testStartedAt = [DateTime]::UtcNow
            $safeTest = $test -replace '[^A-Za-z0-9_.-]', '_'
            $logPath = Join-Path $evidenceDirectory (
                'iteration-{0:D4}-{1}.log' -f $iteration, $safeTest
            )
            Write-Host "WHPX soak iteration ${iteration}: $test"

            $testExitCode = -1
            $previousErrorActionPreference = $ErrorActionPreference
            try {
                $ErrorActionPreference = 'Continue'
                & cargo test -p a3s-box-cli --test core_smoke $test -- `
                    --ignored --nocapture --test-threads=1 2>&1 |
                    Tee-Object -FilePath $logPath
                $testExitCode = $LASTEXITCODE
            }
            finally {
                $ErrorActionPreference = $previousErrorActionPreference
            }
            $residual = @(Wait-ForBoxProcessesToExit -TimeoutSeconds 10)
            $testFinishedAt = [DateTime]::UtcNow
            $passed = $testExitCode -eq 0 -and $residual.Count -eq 0

            $samples += [ordered]@{
                iteration = $iteration
                test = $test
                result = if ($passed) { 'pass' } else { 'fail' }
                exit_code = $testExitCode
                started_at = $testStartedAt.ToString('o')
                finished_at = $testFinishedAt.ToString('o')
                duration_seconds = [Math]::Round(
                    ($testFinishedAt - $testStartedAt).TotalSeconds,
                    3
                )
                residual_processes = @($residual)
                log = $logPath
            }
            $completedTests++
            Write-Summary

            if ($testExitCode -ne 0) {
                throw "WHPX soak test $test failed with exit code $testExitCode"
            }
            if ($residual.Count -gt 0) {
                $description = ($residual | ForEach-Object {
                    '{0}:{1}' -f $_.ProcessName, $_.Id
                }) -join ', '
                throw "WHPX soak test $test leaked processes: $description"
            }
        }
        $completedIterations = $iteration
        Write-Summary
    }

    $result = 'pass'
}
catch {
    $result = 'fail'
    $failure = $_.Exception.Message
}
finally {
    Set-Location -LiteralPath $repositoryRoot
    Write-Summary
}

Write-Host "Windows WHPX soak result: $result"
Write-Host "Evidence: $evidenceDirectory"
if ($failure) {
    throw $failure
}
