# Third-party notices

`a3s-libkrun-sys` is an aggregate package. The `license = "MIT"` field in
`Cargo.toml` covers the A3S Rust wrapper code; it does not relicense the native
libraries, firmware, or embedded Linux kernel distributed with the package.

The package contains or redistributes these components:

| Component | License | Source and license text |
| --- | --- | --- |
| A3S `a3s-libkrun-sys` wrapper | MIT | `LICENSE`, `licenses/MIT.txt` |
| A3S/containers `libkrun` fork, including the Windows wrapper around the kernel bundle | Apache-2.0 | `vendor/libkrun-source.tar`, `licenses/Apache-2.0.txt` |
| `libkrunfw` build sources and tooling used to produce the kernel bundle | GPL-2.0-only and LGPL-2.1-only, as marked upstream | `licenses/GPL-2.0-only.txt`, `licenses/LGPL-2.1-only.txt`, and the corresponding-source release asset described in `SOURCE-PROVENANCE.md` |
| Linux kernel embedded in `libkrunfw.dll` | GPL-2.0-only | `licenses/GPL-2.0-only.txt` and the Linux source release asset described in `SOURCE-PROVENANCE.md` |
| EDK2 firmware image used by the aarch64 libkrun source build | See the EDK2 notices | `licenses/EDK2.txt`, `licenses/EDK2-Sources.txt`, and the copies inside `vendor/libkrun-source.tar` |

The exact binary-to-source mapping, checksums, and immutable source locations
are recorded in `SOURCE-PROVENANCE.md`. Redistributors must preserve these
notices and satisfy the applicable licenses for the native artifacts they
redistribute.
