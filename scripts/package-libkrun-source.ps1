[CmdletBinding()]
param(
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$nestedRoot = Join-Path $repositoryRoot 'src/deps/libkrun-sys/vendor/libkrun'
$destination = Join-Path $repositoryRoot 'src/deps/libkrun-sys/vendor/libkrun-source.tar'
$buildScript = Join-Path $repositoryRoot 'src/deps/libkrun-sys/build.rs'
$provenanceFile = Join-Path $repositoryRoot 'src/deps/libkrun-sys/SOURCE-PROVENANCE.md'
$temporary = Join-Path $env:TEMP (
    'a3s-libkrun-source-{0}.tar' -f [Guid]::NewGuid().ToString('N')
)

if (-not (Test-Path -LiteralPath (Join-Path $nestedRoot 'Cargo.toml') -PathType Leaf)) {
    throw "Initialized libkrun submodule is required at $nestedRoot"
}

$nestedStatus = @(
    & git -C $nestedRoot status --porcelain=v1 --untracked-files=all
)
if ($LASTEXITCODE -ne 0) {
    throw "git status failed with exit code $LASTEXITCODE."
}
if ($nestedStatus.Count -ne 0) {
    throw 'Refusing to package libkrun with staged, unstaged, or untracked changes.'
}

# The packaged Windows firmware wrapper was built from commit 2692169. Keep an
# exact corresponding-source snapshot in newer tooling revisions and fail if
# any executable source in that snapshot drifts from the pinned Git blobs.
$correspondingSourceBlobs = [ordered]@{
    'corresponding-source/2692169/Cargo.toml' = '602af33a35dd67933ada726e5829bf4ba3a8a545'
    'corresponding-source/2692169/build.rs' = '8ae91913ea4a7200680d8887fd75fe1b811ce647'
    'corresponding-source/2692169/src/lib.rs' = '7b689f7b81d1b65eff70acf82a7951487fd0a4e4'
}
foreach ($entry in $correspondingSourceBlobs.GetEnumerator()) {
    $relativePath = [string]$entry.Key
    $absolutePath = Join-Path $nestedRoot $relativePath
    if (-not (Test-Path -LiteralPath $absolutePath -PathType Leaf)) {
        throw "Pinned corresponding-source file is missing: $relativePath"
    }

    $actualBlob = (& git -C $nestedRoot hash-object -- $relativePath).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "git hash-object failed for $relativePath with exit code $LASTEXITCODE."
    }
    if ($actualBlob -cne [string]$entry.Value) {
        throw "Pinned corresponding-source file $relativePath has Git blob $actualBlob; expected $($entry.Value)."
    }
}

$archivePaths = @(
    '.cargo',
    'AUTHORS',
    'Cargo.lock',
    'Cargo.toml',
    'corresponding-source',
    'LICENSE',
    'Makefile',
    'README.md',
    'edk2/KRUN_EFI.silent.fd',
    'edk2/License.txt',
    'edk2/Sources.txt',
    'include',
    'init/init.c',
    'init/jsmn.h',
    'libkrun.pc.in',
    'scripts',
    'src',
    'third_party',
    'krun-sys-windows/Cargo.toml',
    'krun-sys-windows/build.rs',
    'krun-sys-windows/src'
)

try {
    & git -C $nestedRoot archive --format=tar --prefix=libkrun/ "--output=$temporary" HEAD @archivePaths
    if ($LASTEXITCODE -ne 0) {
        throw "git archive failed with exit code $LASTEXITCODE."
    }

    $generatedHash = (Get-FileHash -LiteralPath $temporary -Algorithm SHA256).Hash
    $generatedHashLower = $generatedHash.ToLowerInvariant()
    $commit = (& git -C $nestedRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "git rev-parse failed with exit code $LASTEXITCODE."
    }

    if ($Check) {
        if (-not (Test-Path -LiteralPath $destination -PathType Leaf)) {
            throw "Bundled source archive is missing: $destination"
        }
        $existingHash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
        if ($existingHash -cne $generatedHash) {
            throw "Bundled source archive is stale: expected $generatedHash, found $existingHash."
        }
        $buildContents = Get-Content -LiteralPath $buildScript -Raw
        $buildHashMatches = [regex]::Matches(
            $buildContents,
            '(?m)^\s*const\s+LIBKRUN_SOURCE_ARCHIVE_SHA256:\s*&str\s*=\s*(?:\r?\n\s*)?"(?<hash>[0-9a-f]{64})";\s*$'
        )
        if ($buildHashMatches.Count -ne 1) {
            throw 'build.rs must define exactly one lowercase 64-digit LIBKRUN_SOURCE_ARCHIVE_SHA256 constant.'
        }
        $pinnedBuildHash = $buildHashMatches[0].Groups['hash'].Value
        if ($pinnedBuildHash -cne $generatedHashLower) {
            throw "build.rs does not pin bundled source SHA256 $generatedHash."
        }

        $provenanceContents = Get-Content -LiteralPath $provenanceFile -Raw
        $provenanceHashMatches = [regex]::Matches(
            $provenanceContents,
            '(?s)`vendor/libkrun-source\.tar`\s*\(SHA-256\s*`(?<hash>[0-9a-f]{64})`\)'
        )
        if ($provenanceHashMatches.Count -ne 1) {
            throw 'SOURCE-PROVENANCE.md must identify exactly one SHA-256 for vendor/libkrun-source.tar.'
        }
        $pinnedProvenanceHash = $provenanceHashMatches[0].Groups['hash'].Value
        if ($pinnedProvenanceHash -cne $generatedHashLower) {
            throw "SOURCE-PROVENANCE.md does not pin bundled source SHA256 $generatedHash."
        }

        $provenanceCommitMatches = [regex]::Matches(
            $provenanceContents,
            '(?s)deterministic archive was generated from local tooling commit\s*`(?<commit>[0-9a-f]{40})`'
        )
        if ($provenanceCommitMatches.Count -ne 1) {
            throw 'SOURCE-PROVENANCE.md must identify exactly one tooling commit for vendor/libkrun-source.tar.'
        }
        $pinnedProvenanceCommit = $provenanceCommitMatches[0].Groups['commit'].Value
        if ($pinnedProvenanceCommit -cne $commit) {
            throw "SOURCE-PROVENANCE.md pins tooling commit $pinnedProvenanceCommit; expected $commit."
        }
        Write-Output "libkrun source archive OK: commit=$commit sha256=$generatedHash"
        return
    }

    Copy-Item -LiteralPath $temporary -Destination $destination -Force
    Write-Output "Wrote $destination"
    Write-Output "libkrun commit: $commit"
    Write-Output "SHA256: $generatedHash"
    Write-Output 'Update LIBKRUN_SOURCE_ARCHIVE_SHA256 in build.rs when the digest changes.'
}
finally {
    if (Test-Path -LiteralPath $temporary -PathType Leaf) {
        Remove-Item -LiteralPath $temporary -Force
    }
}
