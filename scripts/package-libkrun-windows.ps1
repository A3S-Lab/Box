[CmdletBinding()]
param(
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$prebuiltRoot = Join-Path $repositoryRoot 'src/deps/libkrun-sys/prebuilt/x86_64-pc-windows-msvc'
$destination = Join-Path $repositoryRoot 'src/deps/libkrun-sys/vendor/krun-windows-x64.tar.xz'
$buildScript = Join-Path $repositoryRoot 'src/deps/libkrun-sys/build.rs'
$temporaryArchive = Join-Path $env:TEMP (
    'a3s-libkrun-windows-{0}.tar.xz' -f [Guid]::NewGuid().ToString('N')
)
$temporaryExtract = Join-Path $env:TEMP (
    'a3s-libkrun-windows-check-{0}' -f [Guid]::NewGuid().ToString('N')
)
$runtimeFiles = @('krun.dll', 'krun.lib', 'libkrunfw.dll')

foreach ($name in $runtimeFiles) {
    $path = Join-Path $prebuiltRoot $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Windows runtime file is missing: $path"
    }
    $item = Get-Item -LiteralPath $path -Force
    if ($item.LinkType) {
        throw "Windows runtime file must not be a link: $path"
    }
}

try {
    if ($Check) {
        if (-not (Test-Path -LiteralPath $destination -PathType Leaf)) {
            throw "Bundled Windows runtime archive is missing: $destination"
        }
        $archive = $destination
    }
    else {
        # bsdtar's ustar writer plus a fixed timestamp and ordered input list
        # makes repeated builds with the same toolchain byte-for-byte stable.
        & tar -cJf $temporaryArchive --format ustar --mtime '1970-01-01 00:00:00Z' `
            -C $prebuiltRoot @runtimeFiles
        if ($LASTEXITCODE -ne 0) {
            throw "Windows runtime archive failed with exit code $LASTEXITCODE."
        }
        $archive = $temporaryArchive
    }

    $entries = @(& tar -tf $archive)
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to inspect Windows runtime archive (exit code $LASTEXITCODE)."
    }
    if ($entries.Count -ne $runtimeFiles.Count) {
        throw "Windows runtime archive has $($entries.Count) entries; expected $($runtimeFiles.Count)."
    }
    for ($index = 0; $index -lt $runtimeFiles.Count; $index++) {
        if (-not [string]::Equals(
            [string]$entries[$index],
            [string]$runtimeFiles[$index],
            [StringComparison]::Ordinal
        )) {
            throw "Unexpected Windows runtime archive entry: $($entries[$index])"
        }
    }

    $generatedHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    if ($Check) {
        $buildContents = Get-Content -LiteralPath $buildScript -Raw
        if (-not $buildContents.Contains($generatedHash.ToLowerInvariant())) {
            throw "build.rs does not pin bundled Windows runtime SHA256 $generatedHash."
        }

        $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
        $resolvedExtract = [IO.Path]::GetFullPath($temporaryExtract)
        if (-not $resolvedExtract.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to extract outside the temporary directory: $resolvedExtract"
        }
        New-Item -ItemType Directory -Path $resolvedExtract | Out-Null
        & tar -xf $archive -C $resolvedExtract
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to extract Windows runtime archive (exit code $LASTEXITCODE)."
        }
        foreach ($name in $runtimeFiles) {
            $expected = Join-Path $prebuiltRoot $name
            $actual = Join-Path $resolvedExtract $name
            if (-not (Test-Path -LiteralPath $actual -PathType Leaf)) {
                throw "Bundled Windows runtime is missing after extraction: $name"
            }
            $expectedHash = (Get-FileHash -LiteralPath $expected -Algorithm SHA256).Hash
            $actualHash = (Get-FileHash -LiteralPath $actual -Algorithm SHA256).Hash
            if ($actualHash -cne $expectedHash) {
                throw "Bundled Windows runtime is stale for ${name}: expected $expectedHash, found $actualHash."
            }
            if (-not $buildContents.Contains($expectedHash.ToLowerInvariant())) {
                throw "build.rs does not pin bundled Windows runtime file ${name} SHA256 $expectedHash."
            }
        }
        Write-Output "Windows runtime archive OK: sha256=$generatedHash"
        return
    }

    Copy-Item -LiteralPath $temporaryArchive -Destination $destination -Force
    Write-Output "Wrote $destination"
    Write-Output "SHA256: $generatedHash"
    Write-Output 'Update KRUN_WINDOWS_ARCHIVE_SHA256 in build.rs when the digest changes.'
}
finally {
    if (Test-Path -LiteralPath $temporaryArchive -PathType Leaf) {
        Remove-Item -LiteralPath $temporaryArchive -Force
    }
    if (Test-Path -LiteralPath $temporaryExtract -PathType Container) {
        $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
        $resolvedExtract = [IO.Path]::GetFullPath($temporaryExtract)
        if (-not $resolvedExtract.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove a non-temporary directory: $resolvedExtract"
        }
        Remove-Item -LiteralPath $resolvedExtract -Recurse -Force
    }
}
