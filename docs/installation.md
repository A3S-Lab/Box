# Install A3S Box

A3S Box provides one installer contract through native launchers for each host:

- `install.sh` for Linux and macOS, without requiring PowerShell; and
- `install.ps1` for Windows, without requiring Bash.

Both launchers detect the host, resolve a GitHub release, verify its SHA-256
digest before extraction, validate the package layout and reported version,
then replace a previous installer-managed copy as one staged operation.

## Supported release targets

| Host | Architecture | Release target |
| --- | --- | --- |
| Linux | x86_64 | `linux-x86_64` |
| Linux | arm64 | `linux-arm64` |
| macOS | Apple Silicon | `macos-arm64` |
| Windows | x86_64 | `windows-x86_64` |

Intel macOS and Windows on ARM are rejected rather than receiving an
incompatible archive. The installer installs the runtime distribution; it does
not enable KVM, HVF, WHPX, or Linux Sandbox host capabilities. Review
[Platform boundaries](../README.md#platform-boundaries) and the
[Windows WHPX guide](windows-whpx.md) before running real workloads.

## One-line installation

Linux or macOS:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh
```

Windows PowerShell 5.1 or newer:

```powershell
irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1 | iex
```

Open a new terminal if the current one does not see the updated `PATH`, then
verify the result:

```text
a3s-box --version
a3s-box info
```

Piping code from the network executes the current `main` branch. To inspect it
first, download `install.sh` or `install.ps1`, review the file, and invoke it
locally.

Homebrew remains available on Linux and macOS:

```bash
brew install a3s-lab/tap/a3s-box
```

## Version and destination controls

The default destinations are:

- `${XDG_DATA_HOME}/a3s-box` when `XDG_DATA_HOME` is set;
- `$HOME/.local/share/a3s-box` on other Linux and macOS hosts; and
- `%LOCALAPPDATA%\Programs\A3S Box` on Windows.

The release stays self-contained at that location so its sibling libraries and
guest executable are not separated from `a3s-box`.

| Behavior | Linux/macOS | Windows |
| --- | --- | --- |
| Pin a release | `--version v3.2.4` | `-Version v3.2.4` |
| Choose destination | `--install-dir PATH` | `-InstallDir PATH` |
| Do not change `PATH` | `--no-modify-path` | `-NoModifyPath` |
| Replace an unmanaged directory | `--force` | `-Force` |
| Install a local package | `--archive FILE --sha256 HEX` | `-ArchivePath FILE -Sha256 HEX` |

Pass Unix options after `sh -s --` when using a pipe:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh |
  sh -s -- --version v3.2.4 --no-modify-path
```

Invoke the downloaded PowerShell text as a script block when options are
needed:

```powershell
$installer = irm https://raw.githubusercontent.com/A3S-Lab/Box/main/install.ps1
& ([scriptblock]::Create($installer)) -Version v3.2.4 -NoModifyPath
```

The corresponding environment variables are `A3S_BOX_VERSION`,
`A3S_BOX_INSTALL_DIR`, `A3S_BOX_ARCHIVE`, and `A3S_BOX_SHA256`. Unix also
supports `A3S_BOX_PROFILE` or `--profile` to select the shell startup file.
Set `A3S_BOX_NO_MODIFY_PATH=true` to suppress PATH changes in a piped
invocation. Set `GITHUB_TOKEN` when authenticated GitHub API access is needed
because of rate limits.

## Offline installation

An offline install must supply the release tag and a trusted SHA-256 value. The
archive must retain its published root directory and target-specific layout.

Linux or macOS:

```bash
sh ./install.sh \
  --version v3.2.4 \
  --archive ./a3s-box-v3.2.4-linux-x86_64.tar.gz \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

Windows:

```powershell
.\install.ps1 `
  -Version v3.2.4 `
  -ArchivePath .\a3s-box-v3.2.4-windows-x86_64.zip `
  -Sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

Replace the example digest with the value published for that release. The
online path reads the digest from GitHub release metadata and falls back to the
asset's `.sha256` manifest. It never installs a download that lacks a valid
digest.

## Upgrade and ownership behavior

Every successful install writes `.a3s-box-install.json` inside the destination.
A later run may replace a directory carrying that marker. A non-empty directory
without the marker is left untouched unless `--force` or `-Force` is explicit.
Force never permits replacing a filesystem root, user-profile root, or known
system directory.

The new package is copied to a sibling staging directory and its executable is
checked before activation. If activation or the final version check fails, the
previous managed directory is restored.

On Unix, the installer manages one marked block in the selected shell profile.
On Windows, it adds the installation directory to the per-user `PATH` without
using `setx`, so an existing long `PATH` is not truncated.

## Uninstall

Delete the exact installation directory shown by the installer. On Unix,
remove the block between `# >>> a3s-box installer >>>` and
`# <<< a3s-box installer <<<` from the shell profile it reported. On Windows,
remove that same installation directory from the user `PATH`. Runtime state
stored elsewhere is intentionally not deleted by uninstalling the binaries.
