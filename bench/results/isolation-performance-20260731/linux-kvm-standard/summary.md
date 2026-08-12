# A3S Box isolation-mechanism benchmark summary

| Host lane | Isolation | Mechanism | Pass/total | p50 | p95 | p50 rate/value |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| a3s-os-linux-x86_64-kvm | microvm | bind_metadata_create_delete | 5/5 | 3420.061 ms | 3721.514 ms | 584.785 files/s |
| a3s-os-linux-x86_64-kvm | microvm | bind_warm_read | 5/5 | 364.420 ms | 364.748 ms | 702.485 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | bind_write | 5/5 | 665.365 ms | 2059.031 ms | 384.751 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | bridge_host_http_64k_downloads | 5/5 | 6327.638 ms | 6331.987 ms | 10.114 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | cold_noop_lifecycle | 20/20 | 2219.124 ms | 2373.740 ms | 0.441 operations/s |
| a3s-os-linux-x86_64-kvm | microvm | cpu_sha256 | 5/5 | 1566.515 ms | 1566.973 ms | 163.420 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | exec_noop | 30/30 | 113.943 ms | 114.198 ms | 8.776 operations/s |
| a3s-os-linux-x86_64-kvm | microvm | host_http_64k_downloads | 5/5 | 4925.115 ms | 5075.599 ms | 12.995 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | host_http_64k_downloads_first_attempt_failures | 5/5 | — | — | 0.000 requests |
| a3s-os-linux-x86_64-kvm | microvm | host_http_requests | 5/5 | 314.338 ms | 314.371 ms | 159.064 requests/s |
| a3s-os-linux-x86_64-kvm | microvm | host_http_requests_first_attempt_failures | 5/5 | — | — | 0.000 requests |
| a3s-os-linux-x86_64-kvm | microvm | idle_memory | 1/1 | — | — | 81395712.000 bytes |
| a3s-os-linux-x86_64-kvm | microvm | memory_zero_copy | 5/5 | 113.987 ms | 114.127 ms | 4491.726 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | named_volume_warm_read | 5/5 | 365.766 ms | 615.198 ms | 699.902 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | named_volume_write | 5/5 | 767.865 ms | 2677.412 ms | 333.392 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | parallel_4_cold_noop | 5/5 | 4434.586 ms | 4931.511 ms | 0.902 operations/s |
| a3s-os-linux-x86_64-kvm | microvm | pool_cold_fill | 1/1 | 6829.389 ms | 6829.389 ms | 0.586 VMs/s |
| a3s-os-linux-x86_64-kvm | microvm | preflight_noop | 1/1 | 2620.069 ms | 2620.069 ms | 0.382 operations/s |
| a3s-os-linux-x86_64-kvm | microvm | rootfs_warm_read | 5/5 | 364.512 ms | 618.332 ms | 702.309 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | rootfs_write | 5/5 | 715.583 ms | 2071.222 ms | 357.750 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | snapshot_fork_fill | 3/3 | 3019.657 ms | 3120.338 ms | 1.325 VMs/s |
| a3s-os-linux-x86_64-kvm | microvm | tee_simulated_lifecycle | 10/10 | 2320.437 ms | 2873.861 ms | 0.431 operations/s |
| a3s-os-linux-x86_64-kvm | microvm | tmpfs_warm_read | 5/5 | 114.039 ms | 114.381 ms | 2244.852 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | tmpfs_write | 5/5 | 214.339 ms | 2267.429 ms | 1194.372 MiB/s |
| a3s-os-linux-x86_64-kvm | microvm | volume_backed_init | 10/10 | 2267.988 ms | 2471.738 ms | 0.431 operations/s |
| a3s-os-linux-x86_64-kvm | microvm | warm_pool_acquire | 20/20 | 2068.165 ms | 2267.913 ms | 0.483 operations/s |
| a3s-os-linux-x86_64-kvm | sandbox | preflight_noop | 0/1 | — | — | — |
