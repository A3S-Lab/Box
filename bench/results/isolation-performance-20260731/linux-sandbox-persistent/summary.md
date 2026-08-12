# A3S Box isolation-mechanism benchmark summary

| Host lane | Isolation | Mechanism | Pass/total | p50 | p95 | p50 rate/value |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | bind_metadata_create_delete | 0/5 | — | — | — |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | bind_warm_read | 5/5 | 114.046 ms | 114.554 ms | 2244.709 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | bind_write | 5/5 | 414.572 ms | 415.433 ms | 617.505 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | cpu_sha256 | 5/5 | 1466.416 ms | 1468.294 ms | 174.575 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | detached_remove | 20/20 | 714.934 ms | 765.961 ms | 1.399 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | detached_start | 20/20 | 1015.514 ms | 1118.608 ms | 0.985 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | exec_noop | 30/30 | 113.898 ms | 114.175 ms | 8.779 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | host_network | 0/1 | — | — | — |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | idle_memory | 1/1 | — | — | 7544832.000 bytes |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | memory_zero_copy | 5/5 | 114.024 ms | 114.395 ms | 4490.300 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | named_volume_warm_read | 5/5 | 113.993 ms | 114.037 ms | 2245.760 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | named_volume_write | 5/5 | 365.203 ms | 368.162 ms | 700.980 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | parallel_4_detached_remove | 5/5 | 2173.949 ms | 2471.681 ms | 1.840 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | parallel_4_detached_start | 5/5 | 1575.065 ms | 1774.013 ms | 2.540 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | persistent_preflight_remove | 1/1 | 765.625 ms | 765.625 ms | 1.306 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | persistent_preflight_start | 1/1 | 1416.993 ms | 1416.993 ms | 0.706 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | rootfs_warm_read | 5/5 | 114.298 ms | 114.416 ms | 2239.751 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | rootfs_write | 5/5 | 415.283 ms | 465.023 ms | 616.447 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | tmpfs_warm_read | 5/5 | 114.053 ms | 118.172 ms | 2244.561 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | tmpfs_write | 5/5 | 214.411 ms | 264.791 ms | 1193.970 MiB/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | volume_backed_init_remove | 10/10 | 565.134 ms | 667.910 ms | 1.767 operations/s |
| a3s-os-linux-x86_64-sandbox-persistent | sandbox | volume_backed_init_start | 10/10 | 965.610 ms | 1115.713 ms | 1.035 operations/s |
