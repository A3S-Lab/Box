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
    [ValidateRange(1, 16384)]
    [int]$MaxRuntimeWorkingSetMiB = 2048,
    [ValidateRange(1, 65536)]
    [int]$MaxRuntimeHandles = 8192,
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
    'real_core_volume_backed_init_script_success_and_failure',
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
if (Test-Path -LiteralPath $evidenceDirectory) {
    throw "Refusing to overwrite an existing evidence directory: $evidenceDirectory"
}
New-Item -ItemType Directory -Path $evidenceDirectory | Out-Null
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

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

    $json = ConvertTo-Json -InputObject $Value -Depth 20
    if ($null -eq $json) {
        $json = 'null'
    }
    Write-Utf8Text -Path $Path -Text $json
}

function Get-RecordValue {
    param(
        [Parameter(Mandatory)]
        [object]$Record,
        [Parameter(Mandatory)]
        [string]$Name
    )

    if ($Record -is [Collections.IDictionary]) {
        if ($Record.Contains($Name)) {
            return $Record[$Name]
        }
        return $null
    }
    $property = $Record.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    $property.Value
}

function ConvertTo-TsvCell {
    param([AllowNull()][object]$Value)

    if ($null -eq $Value) {
        return ''
    }
    if ($Value -is [bool]) {
        return $Value.ToString().ToLowerInvariant()
    }
    ([string]$Value).Replace("`t", ' ').Replace("`r", ' ').Replace("`n", ' ')
}

function Write-RecordsTsv {
    param(
        [Parameter(Mandatory)]
        [string]$Path,
        [Parameter(Mandatory)]
        [string[]]$Columns,
        [Parameter(Mandatory)]
        [AllowEmptyCollection()]
        [object[]]$Rows
    )

    $lines = New-Object 'System.Collections.Generic.List[string]'
    $lines.Add(($Columns -join "`t"))
    foreach ($row in $Rows) {
        $cells = @(
            foreach ($column in $Columns) {
                ConvertTo-TsvCell -Value (
                    Get-RecordValue -Record $row -Name $column
                )
            }
        )
        $lines.Add(($cells -join "`t"))
    }
    Write-Utf8Text -Path $Path -Text (($lines -join "`n") + "`n")
}

function Get-BoxProcesses {
    @(
        Get-Process -Name (
            'a3s-box',
            'a3s-box-shim',
            'a3s-oci',
            'a3s-oci-krun-shim'
        ) -ErrorAction SilentlyContinue |
            ForEach-Object {
                $process = $_
                $processIdentifier = $null
                $processName = $null
                $path = $null
                $startedAt = $null
                $cpu = $null
                $handles = $null
                $workingSet = $null
                try {
                    $processIdentifier = $process.Id
                    $processName = $process.ProcessName
                    $process.Refresh()
                    $path = $process.Path
                    $startedAt = $process.StartTime.ToUniversalTime().ToString('o')
                    $cpu = $process.CPU
                    $handles = $process.Handles
                    $workingSet = $process.WorkingSet64
                }
                catch {
                    # A process may exit while its resource fields are sampled.
                }
                if ($null -ne $processIdentifier -and
                    -not [string]::IsNullOrWhiteSpace($processName)) {
                    [pscustomobject][ordered]@{
                        process_id = $processIdentifier
                        name = "$processName.exe"
                        started_at = $startedAt
                        cpu_seconds = $cpu
                        handles = $handles
                        working_set_bytes = $workingSet
                        executable_path = $path
                    }
                }
            }
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

function ConvertTo-NativeArgument {
    param(
        [AllowEmptyString()]
        [Parameter(Mandatory)]
        [string]$Argument
    )

    if ($Argument.Contains('"')) {
        throw "Native argument contains an unsupported quote: $Argument"
    }
    if ($Argument.Length -eq 0 -or $Argument -match '\s') {
        return '"' + $Argument + '"'
    }
    $Argument
}

function Invoke-SoakTest {
    param(
        [Parameter(Mandatory)]
        [string]$Test,
        [Parameter(Mandatory)]
        [string]$LogPath
    )

    $arguments = @(
        'test',
        '-p', 'a3s-box-cli',
        '--test', 'core_smoke',
        $Test,
        '--',
        '--ignored',
        '--nocapture',
        '--test-threads=1'
    )
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = (Get-Command cargo.exe -ErrorAction Stop).Source
    $startInfo.Arguments = (
        $arguments | ForEach-Object { ConvertTo-NativeArgument -Argument $_ }
    ) -join ' '
    $startInfo.WorkingDirectory = $script:workspace
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.CreateNoWindow = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "Failed to start cargo test for $Test"
    }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $timer = [Diagnostics.Stopwatch]::StartNew()
    [int64]$peakRunnerWorkingSet = 0
    [int64]$peakRuntimeWorkingSet = 0
    [int64]$peakRuntimeHandles = 0
    [int]$peakRuntimeProcesses = 0
    [int]$runtimeSampleCount = 0
    $observedNames = New-Object 'System.Collections.Generic.HashSet[string]'
    $timeoutSeconds = if (
        $Test -eq 'real_core_virtiofs_tar_closes_every_source_file_cleanly'
    ) {
        $VirtiofsTimeoutSeconds + 120
    }
    else {
        $CommandTimeoutSeconds + 120
    }
    $timedOut = $false

    while (-not $process.HasExited) {
        try {
            $process.Refresh()
            $peakRunnerWorkingSet = [Math]::Max(
                $peakRunnerWorkingSet,
                [Math]::Max(
                    [int64]$process.WorkingSet64,
                    [int64]$process.PeakWorkingSet64
                )
            )
        }
        catch {
            # The cargo process can exit between HasExited and Refresh.
        }

        $runtimeProcesses = @(Get-BoxProcesses)
        if ($runtimeProcesses.Count -gt 0) {
            $runtimeSampleCount++
        }
        [int64]$workingSet = 0
        [int64]$handles = 0
        foreach ($runtimeProcess in $runtimeProcesses) {
            $name = [string]$runtimeProcess.name
            if (-not [string]::IsNullOrWhiteSpace($name)) {
                [void]$observedNames.Add($name)
            }
            if ($null -ne $runtimeProcess.working_set_bytes) {
                $workingSet += [int64]$runtimeProcess.working_set_bytes
            }
            if ($null -ne $runtimeProcess.handles) {
                $handles += [int64]$runtimeProcess.handles
            }
        }
        $peakRuntimeProcesses = [Math]::Max(
            $peakRuntimeProcesses,
            $runtimeProcesses.Count
        )
        $peakRuntimeWorkingSet = [Math]::Max(
            $peakRuntimeWorkingSet,
            $workingSet
        )
        $peakRuntimeHandles = [Math]::Max($peakRuntimeHandles, $handles)

        if ($timer.Elapsed.TotalSeconds -ge $timeoutSeconds) {
            $timedOut = $true
            $process.Kill()
            break
        }
        [void]$process.WaitForExit(100)
    }
    $process.WaitForExit()
    $timer.Stop()
    $exitCode = if ($timedOut) { -1 } else { $process.ExitCode }
    $stdout = $stdoutTask.Result
    $stderr = $stderrTask.Result
    $process.Dispose()
    Write-Utf8Text -Path $LogPath -Text (
        "# stdout`n$stdout`n# stderr`n$stderr"
    )

    [pscustomobject][ordered]@{
        exit_code = $exitCode
        timed_out = $timedOut
        duration_milliseconds = $timer.ElapsedMilliseconds
        peak_runner_working_set_bytes = $peakRunnerWorkingSet
        peak_runtime_working_set_bytes = $peakRuntimeWorkingSet
        peak_runtime_handles = $peakRuntimeHandles
        peak_runtime_processes = $peakRuntimeProcesses
        runtime_sample_count = $runtimeSampleCount
        observed_runtime_processes = @($observedNames) -join ','
        stdout = $stdout
        stderr = $stderr
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
$startInventory = @()
$finalInventory = @()
$verification = 'running'
$verificationFailures = @()
$completedTests = 0
$completedIterations = 0
$failure = $null
$result = 'running'

function Get-SampleMaximum {
    param([Parameter(Mandatory)][string]$Name)

    [int64]$maximum = 0
    foreach ($sample in $script:samples) {
        $value = Get-RecordValue -Record $sample -Name $Name
        if ($null -ne $value) {
            $maximum = [Math]::Max($maximum, [int64]$value)
        }
    }
    $maximum
}

function Write-EvidenceTables {
    $columns = @(
        'sequence',
        'iteration',
        'test',
        'result',
        'exit_code',
        'timed_out',
        'started_at',
        'finished_at',
        'duration_milliseconds',
        'peak_runner_working_set_bytes',
        'peak_runtime_working_set_bytes',
        'peak_runtime_handles',
        'peak_runtime_processes',
        'runtime_sample_count',
        'observed_runtime_processes',
        'residual_process_count',
        'log'
    )
    $rows = @()
    $sequence = 0
    foreach ($sample in $script:samples) {
        $sequence++
        $row = [ordered]@{ sequence = $sequence }
        foreach ($column in ($columns | Where-Object { $_ -ne 'sequence' })) {
            $row[$column] = Get-RecordValue -Record $sample -Name $column
        }
        $rows += $row
    }
    Write-RecordsTsv -Path (Join-Path $script:evidenceDirectory 'operations.tsv') `
        -Columns $columns -Rows $rows
    Write-RecordsTsv `
        -Path (Join-Path $script:evidenceDirectory 'resource-samples.tsv') `
        -Columns @(
            'sequence',
            'iteration',
            'test',
            'duration_milliseconds',
            'peak_runner_working_set_bytes',
            'peak_runtime_working_set_bytes',
            'peak_runtime_handles',
            'peak_runtime_processes',
            'runtime_sample_count'
        ) `
        -Rows $rows
}

function Get-VerificationFailures {
    $failures = New-Object 'System.Collections.Generic.List[string]'
    if ($script:result -ne 'pass') {
        $failures.Add("runner result is $($script:result)")
    }
    if ($script:startInventory.Count -ne 0) {
        $failures.Add(
            "start inventory contains $($script:startInventory.Count) runtime processes"
        )
    }
    if ($script:finalInventory.Count -ne 0) {
        $failures.Add(
            "final inventory contains $($script:finalInventory.Count) runtime processes"
        )
    }
    if ($script:completedIterations -le 0) {
        $failures.Add('no complete WHPX soak iteration finished')
    }
    if ($DurationSeconds -eq 0) {
        if ($script:completedIterations -ne $Iterations) {
            $failures.Add(
                "completed iteration count is $($script:completedIterations), " +
                "expected $Iterations"
            )
        }
        $expectedTests = $Iterations * $tests.Count
        if ($script:completedTests -ne $expectedTests) {
            $failures.Add(
                "completed test count is $($script:completedTests), expected $expectedTests"
            )
        }
    }
    if ($script:samples.Count -ne $script:completedTests) {
        $failures.Add(
            "operation sample count is $($script:samples.Count), " +
            "expected $($script:completedTests)"
        )
    }
    foreach ($sample in $script:samples) {
        $test = Get-RecordValue -Record $sample -Name 'test'
        if ((Get-RecordValue -Record $sample -Name 'result') -ne 'pass') {
            $failures.Add(
                "operation failed: $test"
            )
        }
        if ([int](Get-RecordValue -Record $sample `
                -Name 'runtime_sample_count') -le 0) {
            $failures.Add("operation captured no A3S runtime sample: $test")
        }
        if ([int64](Get-RecordValue -Record $sample `
                -Name 'peak_runtime_working_set_bytes') -le 0) {
            $failures.Add("operation captured no A3S runtime working set: $test")
        }
        if ([int64](Get-RecordValue -Record $sample `
                -Name 'peak_runtime_handles') -le 0) {
            $failures.Add("operation captured no A3S runtime handles: $test")
        }
        if ([int](Get-RecordValue -Record $sample `
                -Name 'peak_runtime_processes') -le 0) {
            $failures.Add("operation captured no A3S runtime process: $test")
        }
        if ([int](Get-RecordValue -Record $sample `
                -Name 'residual_process_count') -ne 0) {
            $failures.Add("operation retained residual A3S processes: $test")
        }
    }

    $maxWorkingSet = Get-SampleMaximum -Name 'peak_runtime_working_set_bytes'
    $workingSetLimit = [int64]$MaxRuntimeWorkingSetMiB * 1MB
    if ($script:samples.Count -gt 0 -and $maxWorkingSet -le 0) {
        $failures.Add('no A3S runtime working-set sample was captured')
    }
    if ($maxWorkingSet -gt $workingSetLimit) {
        $failures.Add(
            "peak A3S runtime working set $maxWorkingSet exceeds " +
            "$workingSetLimit bytes"
        )
    }
    $maxHandles = Get-SampleMaximum -Name 'peak_runtime_handles'
    if ($maxHandles -gt $MaxRuntimeHandles) {
        $failures.Add(
            "peak A3S runtime handle count $maxHandles exceeds $MaxRuntimeHandles"
        )
    }
    @($failures)
}

function Write-Summary {
    $finishedAt = [DateTime]::UtcNow
    Write-EvidenceTables
    $summary = [ordered]@{
        schema = 'a3s.box.windows-whpx-soak.v2'
        run_id = $runId
        result = $result
        verification = $verification
        verification_failures = $verificationFailures
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
        selected_test_count = $tests.Count
        completed_tests = $completedTests
        command_timeout_seconds = $CommandTimeoutSeconds
        virtiofs_timeout_seconds = $VirtiofsTimeoutSeconds
        start_inventory_processes = $startInventory.Count
        final_inventory_processes = $finalInventory.Count
        max_runtime_working_set_bytes = (
            Get-SampleMaximum -Name 'peak_runtime_working_set_bytes'
        )
        max_runtime_working_set_limit_bytes = (
            [int64]$MaxRuntimeWorkingSetMiB * 1MB
        )
        max_runtime_handles = Get-SampleMaximum -Name 'peak_runtime_handles'
        max_runtime_handles_limit = $MaxRuntimeHandles
        selected_tests = $tests
        failure = $failure
        evidence_contract = @(
            'host.json',
            'inventory-start.json',
            'inventory-final.json',
            'operations.tsv',
            'resource-samples.tsv',
            'summary.json',
            'verify.out'
        )
        samples = $samples
    }
    Write-JsonFile -Path (Join-Path $evidenceDirectory 'summary.json') `
        -Value $summary
}

try {
    $preexisting = @(Get-BoxProcesses)
    $startInventory = $preexisting
    Write-JsonFile -Path (Join-Path $evidenceDirectory 'inventory-start.json') `
        -Value @($startInventory)
    if ($preexisting.Count -gt 0) {
        $description = ($preexisting | ForEach-Object {
            '{0}:{1}' -f $_.name, $_.process_id
        }) -join ', '
        throw "Refusing to start with active A3S Box processes: $description"
    }

    $operatingSystem = Get-ItemProperty `
        'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    Write-JsonFile -Path (Join-Path $evidenceDirectory 'host.json') `
        -Value ([ordered]@{
            caption = $operatingSystem.ProductName
            display_version = $operatingSystem.DisplayVersion
            version = [Environment]::OSVersion.Version.ToString()
            build_number = $operatingSystem.CurrentBuildNumber
            update_build_revision = $operatingSystem.UBR
            architecture = $env:PROCESSOR_ARCHITECTURE
            logical_processors = [Environment]::ProcessorCount
        })

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

            $testRun = Invoke-SoakTest -Test $test -LogPath $logPath
            $testExitCode = $testRun.exit_code
            $residual = @(Wait-ForBoxProcessesToExit -TimeoutSeconds 10)
            $testFinishedAt = [DateTime]::UtcNow
            $passed = (
                $testExitCode -eq 0 -and
                -not $testRun.timed_out -and
                $residual.Count -eq 0
            )

            $samples += [ordered]@{
                iteration = $iteration
                test = $test
                result = if ($passed) { 'pass' } else { 'fail' }
                exit_code = $testExitCode
                timed_out = $testRun.timed_out
                started_at = $testStartedAt.ToString('o')
                finished_at = $testFinishedAt.ToString('o')
                duration_milliseconds = $testRun.duration_milliseconds
                peak_runner_working_set_bytes = (
                    $testRun.peak_runner_working_set_bytes
                )
                peak_runtime_working_set_bytes = (
                    $testRun.peak_runtime_working_set_bytes
                )
                peak_runtime_handles = $testRun.peak_runtime_handles
                peak_runtime_processes = $testRun.peak_runtime_processes
                runtime_sample_count = $testRun.runtime_sample_count
                observed_runtime_processes = (
                    $testRun.observed_runtime_processes
                )
                residual_process_count = $residual.Count
                residual_processes = @($residual)
                log = $logPath
            }
            $completedTests++
            Write-Summary

            if ($testRun.timed_out) {
                throw "WHPX soak test $test exceeded its outer timeout"
            }
            if ($testExitCode -ne 0) {
                throw "WHPX soak test $test failed with exit code $testExitCode"
            }
            if ($residual.Count -gt 0) {
                $description = ($residual | ForEach-Object {
                    '{0}:{1}' -f $_.name, $_.process_id
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
    $finalInventory = @(Wait-ForBoxProcessesToExit -TimeoutSeconds 20)
    Write-JsonFile -Path (Join-Path $evidenceDirectory 'inventory-final.json') `
        -Value @($finalInventory)
    if ($finalInventory.Count -gt 0 -and $null -eq $failure) {
        $result = 'fail'
        $failure = 'The soak finished with residual A3S runtime processes.'
    }
    $verificationFailures = @(Get-VerificationFailures)
    if ($verificationFailures.Count -eq 0) {
        $verification = 'pass'
        Write-Utf8Text -Path (Join-Path $evidenceDirectory 'verify.out') `
            -Text "PASS`n"
    }
    else {
        $verification = 'fail'
        if ($result -eq 'pass') {
            $result = 'fail'
            $failure = 'Evidence verification failed: ' + (
                $verificationFailures -join '; '
            )
        }
        $verificationText = "FAIL`n" + (
            ($verificationFailures | ForEach-Object { "- $_" }) -join "`n"
        )
        Write-Utf8Text -Path (Join-Path $evidenceDirectory 'verify.out') `
            -Text ($verificationText + "`n")
    }
    Write-Summary
}

Write-Host "Windows WHPX soak result: $result"
Write-Host "Evidence: $evidenceDirectory"
if ($failure) {
    throw $failure
}
