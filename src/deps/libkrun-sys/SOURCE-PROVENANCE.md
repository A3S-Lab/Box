# Native runtime source provenance

This document maps the native Windows runtime shipped by
`a3s-libkrun-sys` to its source and corresponding Linux kernel source. Hashes
are lowercase SHA-256 unless noted otherwise.

## Packaged Windows runtime

The deterministic `vendor/krun-windows-x64.tar.xz` archive has SHA-256
`f528d69284dc4b182851c2bc10ef060512306848f11a4e358a461556d81a7268`
and contains exactly:

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `krun.dll` | 7,428,608 | `3af54645aa675356d631839ec64b863b1ef45ff1d83fb7530ffd39a83b1ea17a` |
| `krun.lib` | 11,870 | `3ac760758158bd4d2d6570db58037d47cd370a8e6ea04ccf54a8b24fd1fdec3d` |
| `libkrunfw.dll` | 21,473,280 | `44f25540f58155c01258fe123617636fdc6cff27873e38e71dbc75f139602077` |

`krun.dll` and its import library correspond to A3S-Lab/libkrun commit
`e50683984386611f9a06d7a66d87976d8aa4bbcb`. The packaged
`libkrunfw.dll` wrapper remains from commit
`2692169b7567363244fdd21cb83de3220ebf3021`. The required source is included
in `vendor/libkrun-source.tar` (SHA-256
`05f6d3137d424e131aafc9cd0fdef6cde019b4ede15b19cacf6435280748588e`).
The deterministic archive was generated from local tooling commit
`e50683984386611f9a06d7a66d87976d8aa4bbcb`; its
`corresponding-source/2692169` directory preserves the exact wrapper source
for the packaged firmware DLL. The archive also contains the Apache-2.0
license and the EDK2 source notices.

## Embedded kernel bundle

Calling `krunfw_get_kernel` in the packaged `libkrunfw.dll` returns 21,364,736
bytes with load address `0x1000000`, entry address `0x1000123`, and SHA-256
`781375ea09f4279ec5bfeab26ecc7067358a3fc98190467e2ab01cc6e98936dd`.

Those bytes are identical to the first 21,364,736 bytes of the
`KERNEL_BUNDLE` symbol in the official upstream libkrunfw v5.5.0 x86_64
library. The final symbol byte is the C array terminator. Verification inputs:

| Input | Immutable identity / SHA-256 |
| --- | --- |
| libkrun/libkrunfw tag | `v5.5.0`, commit `ec4b297964877d83432f9ccda6dad8ff6e9de3e4` |
| Official runtime asset | `https://github.com/libkrun/libkrunfw/releases/download/v5.5.0/libkrunfw-x86_64.tgz` |
| Runtime asset SHA-256 | `c169206b01c89fbe134f1728bf4f988702bc7f73b4cf73e6fdece447d6fceca1` |
| Extracted `lib64/libkrunfw.so.5.5.0` SHA-256 | `6df51f65d7f99fc22215e69a4236c770b1588ceb6777eca014f92b366517d237` |
| Matching configuration | `config-libkrunfw_x86_64`, SHA-256 `ceb2ccebaf279b302f3e2c52b66dc350025d982e23ba653da911188d46f3ba35` |
| Patch series | The 30 patches in the v5.5.0 source, `0001` through `0030` |

The payload matches the upstream generic x86_64 configuration byte for byte;
it must not be described as a clean build of the different historical
`config-libkrunfw-windows_x86_64` configuration.

## Corresponding source

Every `libkrun-sys-v<VERSION>` GitHub release is prepared before the matching
crate is published and carries checksum-verified copies of:

- `a3s-libkrun-source.tar`, the source for the packaged `krun.dll` and Windows
  wrapper;
- `libkrunfw-5.5.0-source.tar.gz`, the upstream v5.5.0 source, generic x86_64
  configuration, complete patch series, and GPL/LGPL license texts;
- `linux-6.12.91.tar.xz`, the complete kernel source used by v5.5.0;
- `libkrunfw-x86_64-v5.5.0.tgz`, the official binary used for the byte-level
  bundle comparison.

The upstream source archive copied into the release is fetched from
`https://github.com/libkrun/libkrunfw/archive/refs/tags/v5.5.0.tar.gz` and is
accepted only with SHA-256
`b0cbf1450269c80aea1dccbf440011deb2762a098b338c234079a6ef06456ead`.
The Linux source is fetched from
`https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.91.tar.xz` and is
accepted only with the kernel.org-published SHA-256
`0ff2ab9e169f9f1948557471fbb450d3018f8c5b77caf288e1a3982582597969`.

GitHub-generated source archives are not promised to remain byte-stable. The
workflow therefore fails closed on a digest change and republishes the verified
bytes as an immutable asset of the A3S release instead of relying on the
generated URL as the long-term distribution location.

## Reproducibility note

The current `krun.dll` build records path-remapping and `/Brepro` controls in
the included source. The exact compiler/container identity used for the
checked-in `libkrunfw.dll` wrapper was not retained. Its wrapper source and
embedded payload are nevertheless identified above independently and shipped
with complete corresponding source. Future firmware refreshes must record the
compiler, binutils, build command, and build-image digest before replacing the
pinned DLL hash.
