# A3S Box isolation-mechanism benchmark summary

| Host lane | Isolation | Mechanism | Pass/total | p50 | p95 | p50 rate/value |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| macos-arm64-hvf | microvm | bind_metadata_create_delete | 4/5 | 1622.845 ms | 1790.009 ms | 1221.522 files/s |
| macos-arm64-hvf | microvm | bind_warm_read | 5/5 | 236.034 ms | 240.505 ms | 1084.590 MiB/s |
| macos-arm64-hvf | microvm | bind_write | 5/5 | 336.375 ms | 390.679 ms | 761.056 MiB/s |
| macos-arm64-hvf | microvm | cold_noop_lifecycle | 20/20 | 2199.156 ms | 2322.047 ms | 0.454 operations/s |
| macos-arm64-hvf | microvm | cpu_sha256 | 5/5 | 1732.496 ms | 1742.060 ms | 147.764 MiB/s |
| macos-arm64-hvf | microvm | exec_noop | 30/30 | 128.928 ms | 181.720 ms | 7.748 operations/s |
| macos-arm64-hvf | microvm | host_http_64k_downloads | 4/5 | 2832.124 ms | 3096.920 ms | 21.381 MiB/s |
| macos-arm64-hvf | microvm | host_http_64k_downloads_first_attempt_failures | 4/4 | — | — | 9.000 requests |
| macos-arm64-hvf | microvm | host_http_requests | 0/5 | — | — | — |
| macos-arm64-hvf | microvm | idle_memory | 1/1 | — | — | 135593984.000 bytes |
| macos-arm64-hvf | microvm | memory_zero_copy | 5/5 | 127.075 ms | 129.903 ms | 4029.118 MiB/s |
| macos-arm64-hvf | microvm | named_volume_warm_read | 5/5 | 236.026 ms | 238.788 ms | 1084.628 MiB/s |
| macos-arm64-hvf | microvm | named_volume_write | 5/5 | 343.855 ms | 395.328 ms | 744.501 MiB/s |
| macos-arm64-hvf | microvm | parallel_4_cold_noop | 5/5 | 3188.025 ms | 3332.489 ms | 1.255 operations/s |
| macos-arm64-hvf | microvm | pool_cold_fill | 1/1 | 6808.582 ms | 6808.582 ms | 0.587 VMs/s |
| macos-arm64-hvf | microvm | preflight_noop | 1/1 | 5806.835 ms | 5806.835 ms | 0.172 operations/s |
| macos-arm64-hvf | microvm | rootfs_warm_read | 5/5 | 233.105 ms | 233.791 ms | 1098.220 MiB/s |
| macos-arm64-hvf | microvm | rootfs_write | 5/5 | 451.577 ms | 1408.107 ms | 566.902 MiB/s |
| macos-arm64-hvf | microvm | snapshot_fork_fill | 3/3 | 29131.279 ms | 29162.255 ms | 0.137 VMs/s |
| macos-arm64-hvf | microvm | tee_simulated_lifecycle | 10/10 | 2166.934 ms | 2304.599 ms | 0.455 operations/s |
| macos-arm64-hvf | microvm | tmpfs_warm_read | 5/5 | 129.136 ms | 132.877 ms | 1982.414 MiB/s |
| macos-arm64-hvf | microvm | tmpfs_write | 5/5 | 185.253 ms | 237.673 ms | 1381.892 MiB/s |
| macos-arm64-hvf | microvm | volume_backed_init | 10/10 | 2202.373 ms | 2400.382 ms | 0.445 operations/s |
| macos-arm64-hvf | microvm | warm_pool_acquire | 20/20 | 1953.253 ms | 2096.862 ms | 0.511 operations/s |
