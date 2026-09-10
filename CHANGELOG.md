# Changelog

All notable changes to A3S Box will be documented in this file.

## [Unreleased]

### Added

- Observation harness `a3s.box.linux-native-live-session.v4` extends the
  Native Linux live-session gate with filesystem continuity across owner
  SIGKILL via public Box `transfer_file` (upload before kill, download after
  reattach on the same generation; `retained_filesystem_proven`). Keeps
  retained streaming handle proof from v3. Does **not** close B2
  (`b2_process_session_recovery_closed` stays false) and does not claim
  fixture `process_restart` continuity. **Existing-host WSL2 greened** on
  OCI `61f77712e420c176dfc1a5d7ba2457e8c8299dcf`: report SHA-256
  `3d158d6755afcd187f883234768f54aaa1dbd330e56a2ef763d11164fd6b5818`.

- Existing-host WSL2 `/dev/kvm` greened `a3s.box.linux-kvm-live-session.v1`
  (`retained_stream_handle_proven=true`, `kvm_microvm_live_claimed=true`) on
  Box `0a6ce8d7…` + OCI `f532e2d…` (report SHA-256
  `d3d4f3c81659ba646ebb5218bc57db9bb77d765284713d5d41900001848e705e`).
  `b2_process_session_recovery_closed` stays false.

- Linux KVM MicroVM live-session observation harness
  (`linux-kvm-live-session-qualification`, schema
  `a3s.box.linux-kvm-live-session.v1`) with local runner
  `scripts/linux-kvm-live-session-qualification.sh` and report verifier
  `scripts/verify-linux-kvm-live-session-report.py`. Exercises
  `A3S_OCI_KVM_SESSION_OWNER=1`, retained streaming handle continuity across
  Host Service SIGKILL/restart, and sets `kvm_microvm_live_claimed` only when
  `retained_stream_handle_proven`. Does **not** close B2 or claim utility-VM
  Live; `/dev/kvm` green evidence remains pending.

### Changed

- Bump pinned OCI Runtime to `61f77712e420c176dfc1a5d7ba2457e8c8299dcf`
  (Native Live filesystem continuity, OCI-Runtime #290). Prior pin
  `f532e2d818cc302849a7c92653ca8256e3ba277e` covered KVM Live retained exec I/O.

### Fixed

- Live-session v3 streaming command survives `close_stdin` EOF (`sleep` after
  `while read`) so Kill after Host reopen still targets a live retained handle
  instead of failing as not-live under start-time identity.
- Local SDK smoke / `check-oci-pins` now assert
  `a3s.oci.native-linux-recovery.v6` to match the pinned OCI Runtime
  (`7001ce5…`). The previous `v3` assertion failed the Sandbox pin gate after
  #296 and rejected honest Live reopen fields (`sessionSupervisor`, `execs`)
  that v6 may serialize.
- Native Linux retained-manager Live reopen respawns the identity-fenced OCI
  Host on `inspect`/`reconcile` Unavailable (`with_native_linux_owner_recovery`).
  Without that, the SDK only reconnects to a dead socket and the session
  supervisor's 30s reattach window expires before a replacement Host arrives.
- Native Linux live-session v3 streaming `start_process` omits one-shot
  `request_id` (OCI rejects keyed ids on streaming sessions; matches the
  `process_restart` fixture contract) and uses a long exec timeout so the
  retained-stream watchdog cannot poison the handle during owner respawn.
- Kill completion no longer invents `128+signal` exit codes **or**
  `stopped_by_user` for `AlreadyStopped` / vanished-runtime / NotFound paths.
  Only a true `Killed` outcome uses `KillTerminal` (user stop) and may derive
  a signal exit when the reap is missing; owner-loss cleanup uses `Terminal`
  so exit stays absent and user-stop is not fabricated. Contract tests cover
  the no-invent paths and use MicroVM isolation so they run on Windows hosts
  as well as Linux.
- Filesystem-only (cold) pause no longer publishes `Paused` when stop fails and
  inspect shows `Failed`, or `Stopped` with an authenticated exit. Those
  generations become `Failed`/`Stopped` with the observed exit. Lost-response
  kill reconciliation likewise publishes crash `Failed` via `Terminal` (not
  user `KillTerminal` / `stopped_by_user`). Clean `Stopped` without exit after
  a failed stop response remains a successful cold pause.
- Filesystem-only (cold) resume no longer rolls terminal `Stopped`/`Failed`
  evidence back to retryable `Paused` (which dropped the exit). Terminal
  generations are published with `Terminal` evidence; `Created`/`Paused`/
  `NotFound` remain retryable rollbacks.
- Warm (in-memory) pause/resume no longer leaves a generation stuck in
  `Pausing`/`Resuming` when inspect shows terminal `Stopped`/`Failed`. The
  authenticated exit is published immediately with an Unavailable refusal,
  matching cold-path honesty instead of deferring only to later reconcile.
  The same immediate `Failed` publish applies when warm pause/resume inspect
  returns NotFound (vanished runtime).
- Inspect/reconcile `observe_record` terminal projection for **in-flight**
  lifecycles now uses `startup_terminal_state` (Stopped without exit → `Failed`,
  not invented clean `Stopped`). Pending `Killing` with an authenticated Stopped
  exit uses `KillTerminal` so `stopped_by_user` matches `finish_kill`. Stable
  `Running`/`Paused` owner-loss Stopped-without-exit stays `Stopped` (OCI
  semantics).

### Changed

- Local FakeBackend lifecycle unit tests default create isolation to MicroVM so
  Windows hosts run the same pause/resume/kill/reconcile honesty contracts
  instead of failing at create with Linux-only Sandbox rejection. Explicit
  Sandbox negatives (Windows reject-Sandbox) still force Sandbox. Filesystem
  snapshot contracts that require the Sandbox backend stay Linux-gated with
  explicit `sandbox_request`. Snapshot recover/reconcile now also refuses to
  publish Sandbox-only filesystem snapshots on non-Sandbox generations (no
  invent-via-recover), restoring the source state instead. Cold-paused
  non-Sandbox abort restores `Paused` when inspect is NotFound or
  Stopped-without-exit (cold pause has no live provider by design); it no
  longer invents `Failed`/`Terminal` for that expected absence. Recover,
  reconcile, and inspect/stabilize treat that abort as a successful restore
  (Ready / restored status) rather than failing the caller with the
  create-time Sandbox-only Conflict after state was already repaired.

### Added

- Native Linux live-session observation harness:
  `linux-native-live-session-qualification` example plus
  `scripts/linux-native-live-session-qualification.sh`. Schema
  `a3s.box.linux-native-live-session.v3` requires
  `A3S_OCI_NATIVE_SESSION_SUPERVISOR=1`, keeps the Box manager across Host
  owner SIGKILL, and is intended to prove retained streaming `start_process`
  handle continuity (Unavailable → Live reopen → same-handle stdin/signal/Exit)
  plus keyed captured exec / state / inventory / stats / kill without inventing
  an exit. `retained_stream_handle_proven` flips true only when that path
  passes; `fixture_stream_continuity_claimed` and
  `b2_process_session_recovery_closed` stay false. Does not claim KVM MicroVM
  Live continuity or close utility-VM B2/R6. Default create stays Host-bound.
  Existing-host WSL2 matched-creds + elevate report SHA-256
  `bc36ff5b895b6320b57322328f78910be82eddfb2203b97bd2c86455b1929d02` on Box
  `2c304534…` + OCI `7001ce5…` (`status=passed`,
  `retained_stream_handle_proven=true`). Does **not** set
  `b2_process_session_recovery_closed` or claim KVM/utility-VM Live /
  default supervised create.
- Linux qualification-only MicroVM OCI vertical-slice gate:
  `linux-kvm-oci-qualification` example plus
  `scripts/linux-kvm-oci-qualification.sh`. Schema
  `a3s.box.linux-kvm-oci-qualification.v2` exercises create replay, Box
  manager reopen, start, exact exit status `23`, delete replay, residual path
  cleanup, and—when service restart inputs are supplied—Host Service
  SIGKILL/restart while a generation is running with stopped-only reconcile
  (no invented exit status). The local runner starts
  `box-kvm-qualification-service` itself. Observation-only; not a fresh-host
  or production-routing claim.
- Linux qualification-only MicroVM OCI opt-in through
  `A3S_BOX_OCI_MIGRATION=microvm|all` plus an explicit
  `A3S_BOX_OCI_KVM_ENDPOINT` Unix socket pointing at OCI Runtime's
  `box-kvm-qualification-service`. The public KVM probe stays non-registerable;
  this path reuses the portable DedicatedVm bundle provider already used by
  WHPX and is not a production claim.
- Windows WHPX docs now include a first-principles acceptance matrix index
  (`WIN-HOST` / `WIN-POS` / `WIN-NEG` / `WIN-SLO`) tied to issues #252, #255,
  and #257.
- Windows CLI coverage for bridge/network-create/pool-start/container-update
  fail-closed negatives (#260).

### Changed

- Bump the pinned OCI Runtime revision to
  `7001ce5a4c32cd6e2bbb9a833fc45fd05d2318c9` (per-process Live stdin deposits
  after Host reopen, PR #279). Existing-host WSL2 matched-creds + elevate
  Native live-session v3 report SHA-256
  `bc36ff5b895b6320b57322328f78910be82eddfb2203b97bd2c86455b1929d02`
  (`status=passed`, `retained_stream_handle_proven=true`, keyed exec
  before/after reopen, live kill, removed). Align CI/release
  `A3S_OCI_RUNTIME_REV` with the Cargo `a3s-oci-sdk` pin. Does **not** close
  B2 (`b2_process_session_recovery_closed` stays false), default supervised
  create, MicroVM cutover, fresh-host, or AArch64.
- Prior pin `07e653f9fb594e7454f6f732f3d3ceb80928b254` greened live-session v2
  (manager-drop) report SHA-256
  `614dcb5dc08572fb9d6166c2ed00f736db4e7508b145283a047569eeb89323db`
  (keyed exec / live kill only; no retained-stream claim).
- Restore the OCI Runtime pin to `402949c2` (last CI-green with Sandbox R17
  PR #268). Hosted SDK Local Sandbox fails on `35c3370` with
  `rootless exec timed out` in OCI `native-linux-smoke`; keep KVM requal
  bumps observation-only until that smoke is green on GitHub-hosted runners.
- Align CI/release `A3S_OCI_RUNTIME_REV` with the Cargo `a3s-oci-sdk` pin
  (`402949c2`) and keep recovery smoke on `a3s.oci.native-linux-recovery.v3`.
- Bump the pinned OCI Runtime revision to
  `35c3370d5aefc1d10c30b77c899044c26865c8c6` (PR #266: new exec after Host
  reopen). Migration stays opt-in via `A3S_BOX_OCI_MIGRATION`; default
  MicroVM/sandbox cutover is unchanged. Re-ran the existing-host WSL2 Linux
  KVM OCI qualification v2 against that pin: exact exit `23`, Host Service
  restart → stopped-only reconcile, report SHA-256
  `de79aab7db7676e08543dbe011019cae5f7ccb32b315756f3b4a7a7f9b7ae5ed`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `019b2d2ee8729886392eae77972d3cc9eaa51bbe` (PR #265: live pause/resume/stats
  on Host reopen). Migration stays opt-in via `A3S_BOX_OCI_MIGRATION`; default
  MicroVM/sandbox cutover is unchanged. Re-ran the existing-host WSL2 Linux
  KVM OCI qualification v2 against that pin: exact exit `23`, Host Service
  restart → stopped-only reconcile, report SHA-256
  `532d1195251a927175d6a55212368f460920be1da682ef2f30209bfcc27873a4`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `fdaf2e712aac2e165c8fb7aa53eb6a96dc462522` (PR #264: authentic
  `wait_process` after Host reopen via supervisor-parented exec). Migration
  stays opt-in via `A3S_BOX_OCI_MIGRATION`; default MicroVM/sandbox cutover is
  unchanged. Re-ran the existing-host WSL2 Linux KVM OCI qualification v2
  against that pin: exact exit `23`, Host Service restart → stopped-only
  reconcile, report SHA-256
  `0746d5c2ee0f059d7a653fbc82294693f85aaeb59e901460c6688f1477d7668d`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `f9d6eeb1c6f0c17e8abe3ed623abd586f48c9ac0` (PR #263: live `signal_process`
  on Host reopen). Migration stays opt-in via `A3S_BOX_OCI_MIGRATION`; default
  MicroVM/sandbox cutover is unchanged. Re-ran the existing-host WSL2 Linux
  KVM OCI qualification v2 against that pin: exact exit `23`, Host Service
  restart → stopped-only reconcile, report SHA-256
  `e6b42c81306cf4784f406b87de388f29c9631ae9cdb17a2a98c889bb803e5d79`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `f5118ff46e7bbd6cdb11ec67dbb9d1a11dfa2677` (PR #262: durable exec inventory
  on Host reopen). Migration stays opt-in via `A3S_BOX_OCI_MIGRATION`; default
  MicroVM/sandbox cutover is unchanged. Re-ran the existing-host WSL2 Linux
  KVM OCI qualification v2 against that pin: exact exit `23`, Host Service
  restart → stopped-only reconcile, report SHA-256
  `99ecfd40bf35a73cba027c325eccb30f76ee3bd3eb316d6be6fedffc82931217`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `26ab4889242c73216ad1b9abadd48367118df028` (PR #261: exclusive stdout/stderr
  Host reopen via supervisor IPC relay). Migration stays opt-in via
  `A3S_BOX_OCI_MIGRATION`; default MicroVM/sandbox cutover is unchanged.
  Re-ran the existing-host WSL2 Linux KVM OCI qualification v2 against that
  pin: exact exit `23`, Host Service restart → stopped-only reconcile, report
  SHA-256
  `576687d960023aea4e3c7599052a4138fec30f643d28a0c7c21232117d3a49f9`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `7d27d1deaa19566a4b51e82032658f6699230112` (PR #260: supervised stdin
  restore on Host reopen). Migration stays opt-in via
  `A3S_BOX_OCI_MIGRATION`; default MicroVM/sandbox cutover is unchanged.
  Re-ran the existing-host WSL2 Linux KVM OCI qualification v2 against that
  pin: exact exit `23`, Host Service restart → stopped-only reconcile, report
  SHA-256
  `a6d666ea674eb01836a6c7e57070762c45a45e633c533d9d56584fe79e11d784`.
  Observation-only; does not close fresh-host, AArch64, live-session gate, or
  default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `49d409383dc0c6214d76aa91250f6f6c0d453eba` (live Host reopen reattach
  #257–#259: wait/kill/delete, shared supervisor cache, init process
  inventory). Migration stays opt-in via `A3S_BOX_OCI_MIGRATION`; default
  MicroVM/sandbox cutover is unchanged. Re-ran the existing-host WSL2 Linux
  KVM OCI qualification v2 against that pin: exact exit `23`, Host Service
  restart → stopped-only reconcile, report SHA-256
  `3266909e1cb5f03a2544443bd27b19180acfa541ed9e15123b0dd10a397416eb`. Observation-only; does not close fresh-host,
  AArch64, live-session gate, or default MicroVM cutover.
- Bump the pinned OCI Runtime revision to
  `f14c9401f92a5fa97be62c54f58925f5fc1a2a51` (merge of supervised create PR
  #253: opt-in Native `Host → Supervisor → Launcher` create; default create
  stays Host-bound). Live reattach remains incomplete in OCI; migration stays
  opt-in and default MicroVM/sandbox cutover is unchanged. Re-ran the
  existing-host WSL2 Linux KVM OCI qualification v2 against that pin: exact
  exit `23`, Host Service restart → stopped-only reconcile, report SHA-256
  `42a8576d93cd5e82fbe5a02ba9dec2958363a08bc8dc91cea06f552077a58edb`.
  Observation-only; does not close fresh-host, AArch64, or default MicroVM
  cutover.
- Bump the pinned OCI Runtime revision to
  `60b18880942d6ed44a73eddfa102ac6ab1361d0c` (includes DedicatedVm containerd
  init/exec Created/Running/Stopped daemon-restart slice,
  `a3s.oci.linux-kvm-containerd-lifecycle.v3`, plus retained existing-host
  release-matrix evidence). Re-ran the existing-host WSL2 Linux KVM OCI
  qualification v2 against that pin: exact exit `23`, Host Service restart →
  stopped-only reconcile, report SHA-256
  `41d2933d8d8a3d9653be195f6849d8893f0055568ce5d94d5ccb56790bf2429c`.
  Observation-only; does not close fresh-host, AArch64, or default MicroVM
  cutover.
- Bump the pinned OCI Runtime revision to
  `95d624f9831b79abf86bc8e369df849054c63eaf` (includes DedicatedVm containerd
  Created/Running/Stopped daemon-restart slice,
  `a3s.oci.linux-kvm-containerd-lifecycle.v2`). Re-ran the existing-host WSL2
  Linux KVM OCI qualification v2 against that pin: exact exit `23`, Host
  Service restart → stopped-only reconcile, report SHA-256
  `1821ba153a77f17c1e4f5b5b130d6e2f7d58f3fb0abfd3f742c5531a68a50630`.
  Observation-only; does not close fresh-host, AArch64, or default MicroVM
  cutover.
- Bump the pinned OCI Runtime revision to
  `3cea73d7fe6c53469ceb73ca8876e175255c3fcb` (includes containerd DedicatedVm
  bundle handoff and Linux KVM containerd vertical-slice gate). Re-ran the
  existing-host WSL2 Linux KVM OCI qualification v2 against that pin: exact
  exit `23`, Host Service restart → stopped-only reconcile, report SHA-256
  `832e2d04f06197cfd7e724eee769f061af9f8f80a5ae0b60aaeabf687f5c7dfd`.
  Observation-only; does not close fresh-host, AArch64, or default MicroVM
  cutover.
- Bump the pinned OCI Runtime revision to
  `402949c2d73cabdf5b959113806db9108e918cf1` so Box tracks the Guest Agent
  portable-rootfs FD-root fix plus retained KVM lifecycle/recovery/soak/
  create-reopen evidence on that mainline.

### Fixed

- Align CI/release A3S_OCI_RUNTIME_REV with the Cargo 3s-oci-sdk pin (9d6eeb).

- Linux Sandbox `native-linux-service` owners now migrate into a child of the
  empty delegated cgroup root before exec, matching host-service composition.
  Advertised Runtime profiles can start when the CI harness lives in a sibling
  probe cgroup rather than under the delegated tree. Effective-root launchers
  also assign the service-root parent to the real UID before spawn so
  `prepare_private_directory` succeeds after rootless device-policy drop; R17
  keeps euid 0 (no setpriv elevate wrapper) so inherited `--a3s-box-control-fds`
  remain Unix stream listeners. Private runtime socket readiness accepts the
  real-UID owner (not `geteuid`) so effective-root harnesses can wait for the
  dropped owner endpoint. After owner spawn the controller drops euid/egid to
  that real identity via `setresuid`/`setresgid` (saved IDs stay 0) so Unix SDK
  `SO_PEERCRED` same-UID auth succeeds; prepare-time `A3S_HOME` trees are
  reassigned to the real UID first so lifecycle locks remain usable, and R17
  cleanup can still restore effective root for any leftover root-owned paths.
  Capability probing under effective root plans rootless UID/GID mappings for
  the real UID so the bundle matches the post-bootstrap owner identity. Owner
  identity checks accept the Sandbox OCI launcher executable when its digest
  matches the certified `a3s-oci` artifact (CI installs them as sibling copies).
  Subsequent Sandbox boots restore effective root before overlay mounts so
  multi-case R17 profiles are not stuck without CAP_SYS_ADMIN after the first
  peer-auth drop. Restore only runs when saved UID/GID remain 0 (the
  `setresuid`/`setresgid` contract), so ordinary unit tests that never dropped
  do not hit `seteuid(0)` EPERM; after restore, `A3S_HOME` is reclaimed to the
  current effective owner so Runtime state ownership checks pass. Sandbox exec
  and PTY control sockets under `/tmp/a3s-box-sockets` are chowned to the real
  UID after bind so post-drop readiness heartbeats can connect to mode `0600`
  endpoints; boot-failure cleanup also restores effective root before overlay
  unmount. The controller no longer permanently drops euid after owner spawn —
  it only matches the real UID around OCI SDK connect (`SO_PEERCRED` accept)
  so overlay mounts, TCP endpoint relays, and Runtime state ownership keep
  effective root / capabilities for the full R17 profile set. Temporary
  peer-auth `seteuid` uses `PR_SET_KEEPCAPS` so permitted capabilities survive
  the round-trip. Box trees under `A3S_HOME/boxes/<id>` are chowned with `lchown`
  to the real UID so the durable owner can scan device nodes after its own
  credential drop; recursion skips `sandbox/attachments` bind mounts and
  ignores `EROFS` so R17 read-only volume cases cannot fail the handoff.
  Sandbox log workers clear the setpriv euid/ruid mismatch before exec and
  prepend the shim `lib/` dir to `LD_LIBRARY_PATH` so secure-execution mode
  cannot hide bundled libkrun. Crash-recovery / inspect endpoint checks accept
  real-UID-owned `runtime.sock` under effective-root controllers (same contract
  as private socket readiness).
- Terminal reconcile after utility-VM/Host owner loss no longer fails closed
  when the managed OCI log projection exits before drain. Box treats that as
  stopped-only owner-loss recovery and refuses to invent an exit status from
  undrained Wait evidence.
- Linux/macOS portable MicroVM OCI specs no longer request guest
  `a3s.oci.rootfs-metadata.v1` ownership replay. Same-uid virtio-fs retains
  Host share-root UIDs and refuses guest `chown`; Windows WHPX still opts into
  the portable metadata contract. Bundle publish writes the metadata file only
  when that annotation is present. First-principles unit tests now lock the
  annotation gate: absent or non-contract annotation values omit the file;
  the exact schema value publishes and consumes the Box image manifest on the
  handoff copy only.
- Guest rootfs archives encode FIFOs as zero-length tar special entries instead
  of opening them for read, so `a3s-box diff` succeeds when the writable layer
  contains a FIFO (#265).
- Scale reconciliation replaces terminal create operations for deterministic
  slot IDs instead of dead-ending on `ReconcileOutcome::Failed`, and MicroVM
  startup-terminal paths release the in-process runtime owner so retries cannot
  stick on "already has an in-process runtime owner" (#266).
- CLI `--cpuset-cpus` rejects inverted ranges such as `3-1` before run/create
  (#267), matching resize/update validation.
- Production Box-to-OCI composition under Sandbox CI uses matched setpriv
  credentials (`euid==ruid`) so Unix SDK peer auth matches the rootless owner
  after device-policy bootstrap drops the owner to the real UID. The owner child
  is elevated through `A3S_BOX_CI_SETPRIV_WRAPPER` (euid 0 / non-root ruid) so
  `native-linux-host-service` can install the parent-bound rootless device
  helper on nosuid CI mounts. After the socket is ready, the owner identity
  record is rewritten from `SO_PEERCRED` so sudo/setpriv supervisors are not
  mistaken for the durable `a3s-oci` process. The same matched/elevate pair is
  used for the no-KVM installed-product smoke, and owner-death evidence expects
  the sandbox UID rather than host root. The CI cgroup root `cgroup.procs` is
  owned by the sandbox identity so owner migration into a delegated child still
  works. Composition also chowns `A3S_HOME` to that identity after earlier
  euid-0 steps so the matched harness can write state and sockets. The local
  SDK Sandbox smoke skips host-bridge network prune under Sandbox isolation
  because matched credentials lack `CAP_NET_ADMIN`.
- Production Box-to-OCI host-service composition migrates the owner into a
  fresh child under `A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT` in `pre_exec` so
  rootless open sees a host-owned child while the CI harness remains in the
  sibling probe cgroup. Owner-child `cgroup.procs` / `cgroup.subtree_control`
  are chowned for setpriv `access(W_OK)` after the credential drop.
- SDK Local Sandbox CI prepares a delegated cgroup tree and runs owners under
  `setpriv` (non-root real UID/GID, effective root) so rootless device-policy
  bootstrap works when the packaged launcher lives on a nosuid `/tmp` mount;
  `A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT` overrides the default systemd path.
- Bump the pinned OCI Runtime revision to `402949c2` so rootless durable
  owners skip detached `open_tree` RO binds after device-policy drop (Box R17
  mounts profile) while host-service owners keep the previous clone path.
- Bump the pinned OCI Runtime revision to `fb517c6f` so
  `native-linux-host-service --delegated-cgroup-root` bootstraps rootless
  device policy (required for Sandbox creates through the durable host owner).
- No-KVM qualification packages install `a3s-box-sandbox-oci-launcher` (the
  packaged `a3s-oci` binary under the Sandbox launcher name) so SDK Local
  Sandbox CI can resolve owners without a system libexec path.
- Windows rejects `--network <bridge>` before image pull / Creating box (#259).
- Windows `network create` fail-closes for unsupported bridge networks (#263).
- Windows `pool start` exits immediately instead of hanging after an unsupported
  socket-serving message (#261).
- `container-update --cpus` applies the same WHPX single-vCPU validation as
  `run`/`create`, so invalid counts are not persisted (#262).
- Linux Sandbox OCI owners resolve the setuid launcher through
  `A3S_BOX_SANDBOX_OCI_LAUNCHER`, packaged locations, and
  `/usr/local/libexec/a3s-box-sandbox-oci-launcher` instead of a hardcoded path.
- Unix VM destroy skips guest-control stop delivery when the provider has
  exited or the guest has published a terminal status (including the MicroVM
  console-handoff window where the shim is still alive), so short `--rm` runs
  no longer WARN on the 1s delivery timeout.
- Cold MicroVM boot crash-detection grace is reduced from 250ms to 80ms so
  healthy short workloads are not taxed by a fixed sleep after shim spawn.
- Unix cold boots fail closed when the guest exec heartbeat never arrives
  within `A3S_EXEC_READY_TIMEOUT_MS` (default 15s), matching Windows so a live
  shim without a usable control plane is never marked Ready. Short workloads
  that publish a terminal status before heartbeat still succeed.
- Warm-pool snapshot-fork template socket polling fails after ~1.5s instead of
  a fixed ~5s busy-wait when the trigger socket never appears.
- The warm-pool daemon drains on SIGTERM as well as SIGINT/`pool stop`, matching
  the monitor supervisor, so bench `kill` and service managers no longer leave
  idle shims/mounts behind.
- Pool drain removes the on-disk snapshot-fork template directory
  (`~/.a3s/pool/tpl-*`) after idle VMs are destroyed.
- Warm-pool drain best-effort reaps orphaned box directories when an individual
  VM destroy fails, so a teardown error cannot leave shim/mount leftovers
  without a recovery path.
- Lease release, expired-lease reclaim, and oneshot `pool run` teardown use the
  same destroy-or-reap path, so a failed destroy after the lease/map entry is
  removed cannot permanently orphan a shim/mount/box-dir.
- Snapshot template cleanup also removes the canonical `tpl-<image-hash>`
  directory when template state is Failing or Unavailable, not only Ready.
- CRI cancel guards and sandbox destroy paths best-effort reap orphans when
  destroy fails after the sandbox is removed from the in-memory map.
- Performance reference docs mark the historical Linux foreground-Sandbox and
  warm-pool cleanup blockers as addressed in later tips while retaining the
  2026-07-31 matrix numbers.
- Windows rejects `--isolation sandbox` and `--tee` before box creation /
  image pull, matching other WHPX fail-closed gates (#249).
- `pause` / `unpause` fail closed on Windows with a platform diagnostic instead
  of a misleading “cannot unpause because it is running” path (#253).
- `wait` keeps polling until an authoritative exit code is recorded, so a
  `kill` that leaves `inspect` ExitCode 137 no longer prints `0` (#256).
- Windows stop-delivery and exec-ready progress budgets are raised so healthy
  WHPX boots and short `--rm` workloads are less likely to emit happy-path
  WARNs (#251, #254).
- Windows WHPX docs clarify that `a3s-box info` is authoritative when full
  Hyper-V is enabled even if the standalone `HypervisorPlatform` optional
  feature reports Disabled (#250).

## [3.2.5] — 2026-09-06

### Added

- Soak evidence now records per-capability results and versioned host-resource
  samples, making long-running runtime checks easier to compare across hosts.

### Fixed

- Warm-pool shutdown joins in-progress replenishment before draining idle VMs,
  and destroys owned VMs with bounded concurrency. In-flight request ownership
  is retained until cleanup completes, preventing orphaned shims and sockets.
- Detached CLI health workers now own independent Unix sessions, so terminal
  or job process-group cleanup cannot stop probes for still-running boxes.
- macOS shim builds stage the fully versioned runtime dylibs and aliases
  without rewriting their already-signed install names. Packaging preserves
  valid code signatures instead of producing libraries rejected by the loader.
- CRI `RunPodSandbox` smoke requests now use a bounded 120-second
  cancellation deadline so cold MicroVM boot and guest-native rootfs assembly
  are not cancelled by a short client default.
- OCI-only runtime builds keep cleanup and socket-layout support without pulling
  hypervisor modules into the build; optional scale and Compose test features
  remain correctly wired.
- Orphan-sweep integration checks require `/proc` liveness evidence before
  reclaiming a process, while non-Linux hosts use the safe unit contract.

## [3.2.4] — 2026-09-05

### Added

- Lazy pool creation now waits only for its first ready VM; remaining `min_idle`
  capacity is replenished immediately in the background, reducing first-request
  latency for multi-image workloads.
- Lazy warm-pool initialization is now serialized per exact image/resource
  shape, allowing unrelated pools to start concurrently while shutdown fencing
  prevents a pool that finishes initializing during drain from being published.
- VM boot observability now records bounded layout, specification, rootfs,
  launch, and readiness phase histograms, including warm-pool fills.
- Warm-pool boot pressure now exposes an in-flight gauge and a failure counter
  so operators can correlate host CPU saturation with concurrent boot work.
- Failed background warm-pool replenishment now backs off exponentially (up to
  five minutes) to avoid retry storms during provider outages.
- The pool daemon now shares its boot limiter across image pools and on-demand
  misses, preventing multi-image workloads from multiplying host boot bursts.
- Explicit `--warm image=count` entries are now validated against `--max` before
  any VM boots, keeping the per-image pool capacity bounded and avoiding wasted
  startup work after a sizing typo.
- Warm-pool metrics now expose initial-fill duration, including first-ready
  latency for lazy pools, so deployment startup regressions can be measured.
- Compose startup now prefetches unique service images with a bounded
  concurrency of two before creating networks or VMs, reducing serialized
  registry latency while failing cleanly before partial deployment state.
- Compose health-dependency convergence now polls at 500ms instead of 2s,
  reducing avoidable startup delay without changing probe cadence or timeout
  behavior.
- Detached health workers now run their first probe when `start_period` ends,
  instead of adding a full `interval` delay before the first probe.

### Fixed

- Pool socket framing now rejects payloads above 16 MiB and times out partial
  request reads, preventing malformed local clients from causing unbounded
  allocation or permanently occupied connection tasks.
- Pool daemon shutdown now tracks socket handlers and waits up to one minute for
  in-flight requests to finish after draining idle pools.
- Failed multi-image pool startup now rolls back already-created pools, stops
  the temporary socket server, and removes its socket before returning an error.
- Warm-pool shutdown now detaches pooled and leased VMs before asynchronous
  teardown, so registry and pool locks are not held for the full destroy time.
- CRI PodSandbox startup now defers the agent workload until
  `StartContainer`, validates resource annotations and resource updates
  fail-closed, reclaims stale bridge endpoints after a runtime restart, and
  tears down the streaming listener on shutdown.
- CRI socket startup refuses to replace a live endpoint; stale sockets are
  removed only after an identity-checked probe.
- Warm-pool idle gauges and per-VM resource gauges now stay synchronized with
  acquisition, eviction, rollback, and VM destruction.
- Warm-pool initial fill and replenishment now use a configurable bounded boot
  concurrency (default `2`) to avoid host CPU and memory bursts.
- Image accounting counts shared content once, refreshes read paths from the
  authoritative index, and rejects unsafe cache entries; CLI prune reports
  the actual content bytes reclaimed. Per-digest publication/removal locks
  prevent concurrent pulls and tag removal from leaving a dangling index
  entry.
- Volume data cleanup is serialized with metadata removal, and corrupt JSON
  store snapshots use collision-resistant, no-overwrite quarantine copies.

## [3.2.3] — 2026-09-02

### Fixed

- Legacy `VmLocalExecutionBackend` now derives and persists Sandbox image-owned
  anonymous-volume identities during the durable create reservation. Sandbox
  launches no longer fail the ownership-drift check when the OCI image declares
  a `VOLUME` and the migration router is not enabled.
- Sandbox image planning now receives request-scoped registry credentials before
  the durable reservation, then transfers the zeroizing in-memory handoff to
  the execution boot. Private-registry images with anonymous volumes no longer
  fail during pre-reservation metadata resolution, and credentials are still
  resolved exactly once without entering persistent state.

## [3.2.2] — 2026-09-02

### Fixed

- Clean macOS source builds now keep Homebrew LLVM out of the global dynamic
  loader search path used by libkrun's nested Cargo process. Bindgen continues
  to discover Homebrew libclang through its scoped paths, while nested `rustc`
  keeps the ABI-compatible LLVM bundled with the Rust toolchain instead of
  aborting before compilation with a missing-symbol error.

## [3.2.1] — 2026-09-02

### Added

- **Mount-free guest-native filesystem snapshots on macOS.** Clean stopped
  raw-ext4 generations are captured as private copy-on-write clones under the
  versioned `a3s.box.snapshot-rootfs.v1` contract. The manifest binds snapshot
  and source identities to the ext4 schema, builder, capacity, filesystem UUID,
  and sparse-content digest. Restore verifies both the immutable source and its
  new private clone, persists the resolved OCI defaults and artifact-owned disk
  capacity, and remains valid after the source snapshot is deleted. Legacy
  directory snapshots retain their shared-lower behavior and remain readable.

### Changed

- Native libkrun memory snapshot-fork inputs are now validated as a coherent
  Linux x86_64/KVM-only capability before layout preparation. Unsupported
  explicit launches fail before image, box, RAM, rootfs, or VMM side effects;
  warm pools skip the unavailable attempt and cold-boot without selecting the
  APFS compatibility provider.
- Snapshot deletion now enforces directory-lower references inside the store,
  including SDK callers; only the CLI's explicit `snapshot rm --force` path can
  bypass that protection.

### Fixed

- Published `a3s-box-runtime` packages now retain the audited raw-byte ext4
  writer through the explicit `a3s-box-mkext4` package. Registry consumers no
  longer fall back to upstream `mkext4` and fail to compile the guest-native
  rootfs implementation that passed in-repository tests.
- macOS lifecycle reconciliation now records each shim's native process start
  identity, treats an unreaped zombie as exited, and lets the owning monitor
  reap it before applying restart policy. A short-lived service can no longer
  remain falsely `running` forever or leave a defunct shim behind after its
  guest workload exits.
- The deterministic libkrun source archive now includes the `start_vm` example
  declared by its Windows bindings manifest. Clean macOS release builds can
  parse the archived workspace instead of failing before libkrun compilation.
- Warm-pool Dockerfile `RUN` now mounts an ephemeral guest-native `/dev` and
  binds the guest's standard devices before chroot. Character device numbers
  are no longer persisted through macOS VirtioFS as unusable `0:0` nodes, so
  tools can reliably open `/dev/null`, `/dev/urandom`, and their peers without
  committing runtime devices to image layers.
- Registry push now uploads the exact verified manifest bytes from the local
  OCI layout through the authenticated raw-manifest API. Pretty-printed local
  manifests retain their content digest instead of being silently
  canonicalized to a second digest before post-push verification.
- macOS VirtioFS directory iteration now snapshots entries per open or rewound
  handle, returns synthetic FUSE resume cookies, and gives `fdopendir` its own
  descriptor. Paginated, mutating tree deletion no longer skips unrelated
  entries or double-closes the retained handle, so package-manager cleanup can
  reliably remove large bind-mounted dependency trees.
- Clean Apple Silicon release builds now keep Debian's `arm64` repository name
  separate from clang's `aarch64-linux-gnu` target while preparing libkrun's
  guest-init sysroot, avoiding a deterministic package-index 404 without
  falling back to an older system libkrun.

## [3.2.0] — 2026-08-25

### Fixed

- **Fail-closed OCI isolation before CLI product mutation.** `run` and `create`
  now ask the active migration router to preflight the selected isolation before
  named-volume creation or image-cache access. OCI-routed MicroVM requests
  require a launch-ready `DedicatedVm` driver without falling back to the legacy
  backend, and record creation repeats the mutable capability check before
  durable reservation.
- MicroVM and Compose startup now preserve container-runtime invariants across
  arbitrary images: `--network none` disables networking, completed
  dependencies require an authoritative zero exit status, empty managed
  volumes are serialized while copying image contents and metadata and roll
  back partial copies on failure,
  process environments no longer inherit guest-init controls, effective users
  receive their passwd `HOME`, and `/dev/shm` has the standard secure 64 MiB
  default. Rootless OCI extraction now applies upper layers through restrictive
  lower-layer directories without corrupting final modes, while Apple Silicon
  builds and macOS runtime handoff/process detection handle their native path,
  architecture, dynamic-library, and zombie-process semantics.
- Scale reconciliation now resumes a retained active replica slot until it is
  ready instead of treating the mere presence of a still-starting execution as
  convergence. A retained paused lease is resumed through the canonical local
  lifecycle facade, while idempotent passes still skip slots already running.

### Removed

- **Box-local Lambda lifecycle API.** The unpublished `a3s-box-lambda` crate
  and its unused Lambda-specific workload envelope types are removed. Durable
  tenant-scoped finite execution now belongs to A3S Cloud, which dispatches
  provider-neutral A3S Runtime Tasks through Fleet; Box retains only node-local
  Runtime provider mechanics.
- **Parallel image-build backends and Docker execution dependencies.** The
  `buildkit-vm` CLI backend, automatic backend selector, helper settings,
  direct-build push path, dedicated smoke coverage, and external Docker
  commands in obsolete developer image recipes, benchmarks, and firmware
  maintenance are removed. `a3s-box build` now has one native engine and
  ImageStore; isolated macOS `RUN` uses its warm-pool path, while publication
  uses the existing `a3s-box push` command.

### Added

- **Provider-neutral Service lifecycle enforcement.** Box now pins
  `a3s-runtime` 0.5.0 and advertises its atomic `ServiceLifecycle` contract.
  The Runtime provider evaluates readiness and liveness with independent
  start-period and threshold state, restarts an unhealthy Service at most once
  per public operation according to its durable restart policy, and preserves
  terminal state over stale in-flight probes. Lifecycle intent maps to an
  exact persisted `SIGTERM` grace interval, reserves that complete interval
  beyond the ordinary control timeout, and then force-stops a non-cooperative
  workload. Capability-triggered real-provider cases cover probe separation,
  liveness transition/recovery, cooperative stop, and deadline force-stop.
- **Downstream real-process Runtime provider qualification seam.** The Linux
  Box Runtime driver can be compiled with the explicit
  `runtime-provider-qualification` feature to accept a caller-owned execution
  backend, generation-fenced port connector, and immutable provider-build
  identity. This keeps downstream Use/Gateway lifecycle tests on Box's
  production mapping, durable state, health, endpoint, stop, and removal paths
  without exposing the injection constructor in default release builds.
- **Durable standalone Gateway scaling and live replica endpoints.** The
  machine-facing `scale-api` now journals revision-bound idempotent operations,
  reconciles deterministic stateless service slots through the canonical local
  execution manager, adopts exact workloads after restart, and publishes only
  ready generation-fenced HTTP endpoint relays. Compose ACL templates may
  declare one dynamic `0:<guest-port>` mapping; fixed or multiple mappings fail
  closed, endpoint bind/advertise policy is explicit, and authority-only mode
  cannot be mistaken for workload reconciliation. Scale-down now closes a
  retiring listener before waiting for existing relays to drain, force-closes
  them at a configurable bounded deadline, and removes the execution only
  after that drain phase.
- **Backpressured exact-generation event streams in every native SDK.** Rust
  exposes a `Stream`, synchronous and asynchronous Python expose iterators,
  TypeScript exposes an abortable `AsyncIterable`, and Go exposes a
  context-aware stream with an idempotent `Close`. Each stream composes the
  existing bounded long-poll authority, pins the generation visible at stream
  creation, advances only validated cursors, and terminates instead of silently
  following a restart. Runtime observation now also accepts the paused state
  already supported by the core manager. Unit suites prove batch backpressure,
  paused observation, generation drift, option validation, and cancellation;
  the four real local-Sandbox SDK smokes consume the replay-safe
  `resources-updated` event through both the stream and bounded replay paths.
- **Cross-language exact-generation runtime observability and control.** The
  protocol-v3 machine bridge and the synchronous/asynchronous Python,
  TypeScript, and Go SDKs now expose the Rust Sandbox facade's live process
  inventory, normalized CPU/memory statistics, bounded ordered-event polling,
  and replay-safe partial resource updates. All clients validate event bounds,
  update values, and operation identities before runtime access; observation
  responses must match the exact Sandbox generation. The checked capability
  inventory now contains 52 operations. The four real local-Sandbox SDK smokes,
  including both Python client styles, also require a nonempty process inventory
  and valid runtime statistics, then replay one resource update under the same
  operation identity and require exactly one exact-generation
  `resources-updated` event.
- **Bounded artifact export across all native SDKs.** Rust, synchronous and
  asynchronous Python, TypeScript, and Go can export one guest file with a
  caller-selected limit up to the transport-safe 8 MiB single-frame ceiling,
  backend-bounded reads, stat/read size-change and declared-size validation, and a
  lowercase SHA-256 digest. MicroVM guests enforce the selected limit before
  reading; the shared-kernel adapter retains the OCI Runtime transfer cap and
  rejects a response beyond the selected limit. An optional exact host
  destination uses exclusive creation and never overwrites an existing
  artifact.
- **Linux/KVM MicroVM qualification for the shared A3S Runtime provider.**
  `BoxRuntimeDriver` now runs its complete advertised Base, Recovery,
  Networking, Mounts, Health, Resources, Logs, Exec, Security, and Outputs
  profile set through real MicroVMs, followed by the authenticated private
  registry case and an exact cleanup inventory. Runtime `none` and `service`
  networking disable libkrun TSI interception while retaining explicit vsock
  IPC; TCP Service endpoints use the same generation-fenced connector as the
  Sandbox backend. The connector bounds frames, validates the live execution
  generation before and after connection, and rejects guest protocol errors.
- **Native Linux aarch64 production-owner qualification.** Box revision
  `a16772c3` now runs the same pinned OCI Runtime `a6cdae7` revision on native
  x86_64 and aarch64 Ubuntu hosts, builds the architecture-matched musl guest
  init, and drives the Rust, Python, TypeScript, and Go SDK surfaces through the
  production owner route. Both blocking lanes also kill the exact owner and use
  fresh Box processes to prove stopped-only reconciliation, exact cleanup, endpoint
  rebinding, and next-generation restart. The cancellation replay scenario now
  releases its retained workload with an explicit marker only after the caller
  is cancelled and its durable receipt is observed, removing a host-speed race
  without weakening the replay assertion.
- **Durable fail-closed OCI migration routing.** New managed records can now be
  composed through `LegacyOnly`, `SandboxViaOci`, or `AllViaOci` policy. The
  router stamps `box_vm` or `oci_sdk` before capability preflight, persists it
  with the reservation before launch side effects, dispatches every later
  lifecycle/session/observability/filesystem operation from that record instead
  of the current rollout policy, preserves pre-routing OCI and Box records
  through durable evidence, and never tries the alternate backend after
  failure. The pinned OCI Runtime now also provides the long-lived
  multi-container Native Linux host owner needed by the next production bundle
  wiring slice.
- **Out-of-process OCI owner reconciliation.** A cross-platform contract now
  launches two distinct runtime-owner fixture processes on one local endpoint.
  The first persists the exact runtime generation, starts a streaming exec, and
  is terminated; the second reopens that state while the same
  `LocalExecutionManager`, process stream, and input handle remain alive. The
  observed request exposes owner death without hidden replay, then the retained
  objects reconnect and continue process inventory, stdin, output, close,
  signal, wait, container reconciliation, and cleanup. The cross-process log
  records exactly one create, start, and exec. OCI Runtime independently proves
  the same session operations through its real durable `HostRuntimeService`.
  Real-driver transparent live-session retention and WHPX production
  composition remain open.
- **Native Linux production owner-death recovery.** The blocking SDK gate now
  sends `SIGKILL` to the exact identity-fenced OCI owner while a real Sandbox
  generation is running and proves its launcher and init identities terminate.
  A subsequent synchronous SDK request runs in a fresh Box bridge process,
  rebinds a distinct authenticated owner and socket, and reconciles the exact
  runtime generation as stopped. The OCI adapter accepts unavailable wait
  evidence only after the runtime has authoritatively reported that stopped
  state, persists no fabricated exit code, performs stopped-only deletion and
  cleans the old executor root. Another fresh Box process then restarts exactly
  the next Box and OCI generations; CI retains a versioned JSON evidence
  artifact for the complete transition.
- **Retained local OCI runtime connection recovery.** Box now pins the SDK
  transport that preserves one logical `RuntimeClient` across a broken local
  stream. The request that observes the disconnect still fails without hidden
  replay; a later explicit reconciliation reconnects to the same validated
  endpoint and performs a fresh protocol handshake. A cross-platform backend
  contract restarts the real Windows named-pipe or Unix-socket server behind
  one retained `LocalExecutionManager`, then proves that the original operation
  and runtime generation recover with exactly one create and start. This is
  local transport-server evidence; the separate child-process contract covers
  the process boundary without claiming native-driver qualification.
- **Exact-generation OCI observability and resource control.** The canonical
  execution manager, Rust client, and local Sandbox facade now expose live
  process inventory, normalized CPU/memory stats, bounded ordered-event polls,
  and partial cgroup-backed resource updates through `OciLocalExecutionBackend`.
  Every read binds and rechecks the exact Box/runtime generation; malformed
  targets, counters, or cursors fail closed. Updates require advertised SDK
  capability, compile one complete OCI resource contract, persist an
  `updating_resources` claim before mutation, reuse the same runtime operation
  after a lost response or backend recreation, and atomically update both
  managed restart intent and compatibility state only after acknowledgement.
  A separate immutable create-intent digest keeps the original create key
  replayable after later resource changes, while changed update content under
  one operation key is rejected.
- **Exact-generation OCI process sessions.** The SDK-only backend now routes
  captured and streaming exec, initial and streaming stdin, cursor-checked
  stdout/stderr, process signals and wait, PTY input/resize, exact terminal
  status, and bounded timeout cleanup to the persisted OCI target on Unix and
  Windows. Capability and generation checks fail before mutation, keyed
  one-shot calls replay the same process after a lost response and backend
  recreation, stdin mutations retain their identity until acknowledged, and a
  detached exact-target watchdog prevents caller cancellation from orphaning a
  timed process. Raw process output remains separate from Box's structured log
  store; alternate exec rootfs requests fail closed instead of being silently
  reinterpreted.
- **Replay-safe OCI freezer routing.** The opt-in SDK-only backend now routes
  memory-retaining pause and resume to the exact persisted OCI generation.
  Each cycle durably claims a stable, non-reused mutation identity, binds it to
  the current Box generation, requires the runtime to advertise the operation
  before dispatch, validates unchanged driver/configuration/attachment
  evidence, and reconciles lost responses after backend recreation without
  repeating the mutation.
  Filesystem-only pause remains a stop-and-reprepare lifecycle operation.
- **Versioned Box-to-OCI attachment binding.** The opt-in SDK-only OCI backend
  now requires `a3s.oci.attachments.v1` before product mutation, derives and
  validates one bundle-bound manifest for rootfs, mounts, networking, process
  I/O, secret classifications, and optional runtime extensions, and stores the
  runtime-returned SHA-256 digest in durable binding schema v2. Missing,
  malformed, or drifted attachment evidence fails closed; a create-time drift
  triggers exact-generation runtime cleanup and provider rollback.
- **Recorded multi-platform OCI assembly.** The typed
  `assemble_recorded_build_outputs` boundary deterministically combines two to
  eight revalidated single-platform plan receipts into one OCI image index.
  Inputs must share the exact source and non-platform build intent, manifests
  are sorted by platform, shared blobs are copied once, and invalid input fails
  before publication. Assembly reuses the native output graph validator and
  the sole `ImageStore` commit boundary; it adds no build engine, cache,
  scheduler, queue, journal, manifest store, or publisher.
- **Portable native build-cache hydration.** The typed
  `hydrate_recorded_build_cache` boundary revalidates a Box-native OCI cache
  artifact and imports it through the existing `BuildCache` lock and blob/key
  writer. Repeated imports are idempotent, valid local key conflicts fail
  before mutation, layer bytes are copied, and cache-cap pruning preserves the
  complete imported layer set without adding another cache store, importer
  service, queue, or validator.
- **Caller-authorized Runtime Secret materialization.** The shared
  `BoxRuntimeDriver` can compose one `BoxSecretMaterializer` and advertise
  environment, file, and registry-credential `SecretReferences`. Environment
  and file material requires a canonical private Linux tmpfs root, is mounted
  read-only, survives an explicit restart, and is removed with stale or deleted
  generations. Registry credentials resolve only at an uncached image-pull
  boundary, pass through a token-fenced in-memory handoff, override every
  persistent credential source (including with explicit anonymous auth), and
  are zeroized immediately after the pull. Running-generation recovery never
  rematerializes Secrets. Log reads reauthorize and redact only material that
  can enter the workload. Plaintext is excluded from durable Box state,
  creation intent, OCI configuration, logs, cursors, and credential stores.
- **Windows post-boot command channel.** WHPX boxes expose non-interactive
  `exec`, bidirectional single-file `cp`, `top`, and guest PID-aware
  `stats --no-stream` through a shim-owned local named pipe tunneled over the
  existing guest control connection. Interactive PTY sessions remain
  unsupported on Windows.

### Changed

- **Registry-backed OCI SDK composition.** Box now pins the independently
  published `a3s-oci-core` and `a3s-oci-sdk` 0.3.1 source contract. A bounded
  dependency publisher verifies the matching OCI source tag and exact commit
  before using Box's existing crates.io release channel, so downstream Box
  packages no longer depend on an unpublished Git-only SDK version.

- **Pinned OCI WHPX recovery and runtime-share contract.** The exact A3S OCI
  SDK revision now carries an optional exact init exit result through recorded-
  driver startup reconciliation. Its WHPX candidate accepts only authenticated
  guest evidence bound to the exact generation and durable configuration and
  confines each bundle and one-time handoff to a protected per-generation
  virtio-fs share separate from the guest system root. It commits the stopped
  observation and wait result before serving and keeps an explicit stopped-only
  fallback when valid evidence is unavailable. Box continues to consume only
  the public SDK and does not own the WHPX session, runtime share, recovery
  files, or runtime journal.
- **Single-owner Sandbox resource contract.** `linux.resources` now carries
  exact workload CPU, memory, swap, and PID limits. The pinned A3S OCI Runtime
  derives the control-plane headroom, owns the fixed control/workload cgroup
  topology, performs atomic live updates, and removes both levels. Guest Init
  uses the SDK-defined membership descriptors with read-only cgroupfs, keeping
  long-lived services and control transports outside workload OOM selection.
- **Stronger Windows qualification.** The WHPX runner precompiles its real
  smoke executable, records that binary's checksum, applies an explicit
  inter-test partition-release interval, and includes the command/copy/process
  utility profile in its 12-test real-host matrix.

### Fixed

- **OCI lifecycle identity after a failed scale attempt.** Box now includes the
  exact runtime container target in OCI `create` and `start` operation IDs.
  Replaying one execution remains stable, while a replacement execution for
  the same deterministic Gateway scale slot cannot collide with the failed
  execution retained in OCI Runtime's durable operation journal.
- **Partial OCI live-resource updates.** Box now validates each resource change
  against the complete resulting Sandbox configuration but sends OCI Runtime
  only the fields selected by that operation. CPU quota and period remain one
  atomic pair, while unrelated memory, PID, CPU, and immutable device policy
  are preserved instead of being resent. The deterministic CLI coverage list
  also includes the standalone `scale-api` command.
- **Production-shaped macOS TX-backpressure qualification.** The physical
  Apple Silicon/HVF gate now starts the exact digest-pinned PostgreSQL 17 image
  from Box #204 and completes two sequential SCRAM-SHA-256 authentications and
  queries through one published port. The dependency-free protocol client is
  covered by HMAC/PBKDF2 vectors and a deterministic fake-server round trip;
  the real gate also rejects `ENOBUFS` and guest `NETDEV WATCHDOG` evidence.
- **Windows CLI main-stack exhaustion.** Async command dispatch now allocates
  only the selected handler future instead of combining every handler on the
  process main stack. Debug Windows builds can execute state-heavy commands
  such as `build`, `create`, and `wait` without overflowing before the handler
  starts, and the scratch build plus retained-status wait integrations now run
  in the Windows CI lane.
- **Reproducible native OCI descriptors.** Native builds now write the canonical
  epoch into OCI config and history creation fields because the build contract
  carries no creation clock. Rebuilding identical content, including after
  parent-cache hydration, therefore preserves the exact manifest descriptor.
- **OCI-routed direct argv commands.** Captured exec, streaming exec, and PTY
  sessions now resolve a relative `argv[0]` through the effective container
  `PATH` against the prepared rootfs before SDK dispatch. Native Rust, Python,
  TypeScript, and Go callers keep the documented shell-free `Argv("printf",
  ...)` behavior while A3S OCI Runtime still receives a normalized absolute
  Linux executable path.
- **Sandbox cgroup namespace handoff.** Guest Init now accepts and verifies the
  runtime's pre-isolated control membership after the namespace is rooted at
  management and rejects a populated management envelope. The workload
  descriptor remains the only path used by main, exec, streaming exec, and PTY
  processes.
- **Managed stopped-box start.** `a3s-box start` now restarts stopped or failed
  managed boxes through the generation-fenced restart protocol, preserving the
  Docker-like stop/start contract without reviving a terminal generation.
- **Concurrent warm-pool cold starts.** Linux rootfs provider detection is
  cached after one synchronized overlayfs capability probe, and the 84 KB VM
  boot future is heap-indirected before a pool miss awaits it. Parallel misses
  no longer multiply temporary overlay work or exhaust a Tokio worker stack.
- **First live resource update.** MicroVM containers retain an otherwise-empty
  per-container cgroup from startup, so `container-update` can safely apply the
  first CPU, memory-reservation, swap, PID, or cpuset limit without targeting
  the guest root cgroup or requiring an initial resource limit.
- **Current guest-init qualification.** Linux host integration rebuilds the
  static musl guest init from the current checkout instead of building an
  unusable dynamic host binary and silently selecting an older musl artifact.
- **Sandbox live resource updates.** Running shared-kernel Sandboxes now send
  one complete resource update through the exact-generation A3S OCI SDK.
  Guest-local `box-*` cgroup writes remain exclusive to MicroVMs, so Sandbox
  creation, updates, and cleanup no longer have competing ownership paths.
- **WHPX control readiness and response delivery.** Startup waits for the real
  guest control/exec heartbeat before publishing `running`, short-lived
  workloads recover their terminal status without a false boot failure, and
  named-pipe responses are flushed before disconnect so final protocol frames
  are not lost.
- **Running-rootfs diff baseline.** The VM runtime captures its baseline after
  host preparation and before guest entry, allowing Windows `diff` to report
  post-boot changes without depending on a guest archive channel.

### Added

- **Cloud Runtime contract alignment.** The shared `BoxRuntimeDriver` now pins
  the exact A3S Runtime revision used by A3S Cloud and accepts digest-pinned OCI
  image manifests and multi-platform OCI image indexes without advertising a
  Docker manifest media type.
- **Native Go SDK.** The nested `github.com/A3S-Lab/Box/sdk/go/v3` module
  exposes the complete checked local bridge inventory through context-aware,
  concurrency-safe clients, typed builders, binary-safe commands and files,
  lifecycle generation fencing, and real MicroVM/Sandbox smoke coverage. Go
  releases use the matching path-prefixed `sdk/go/vX.Y.Z` tag.
- **Safe runtime cache reclamation.** `system-prune --all` now reclaims
  unreferenced directory and APFS sparse-image rootfs caches plus image-pull
  work directories whose owner process has exited. Active Box markers, live
  pull owners, publication staging paths, and persistent cache lock files are
  preserved.

### Removed

- **Remote E2B compatibility service.** The pinned upstream protocol corpus,
  official-client fixtures, ACL control/data-plane service, dedicated runtime
  image, release binary, and their CI/release plumbing are removed. Native
  Rust, Python, TypeScript, and Go SDKs remain local, credential-free entry points
  over the core A3S Box runtime.

### Changed

- **A3S OCI Runtime is the sole Sandbox backend.** New shared-kernel Sandbox
  executions use the published `a3s-oci-sdk` `0.3.1` contract; the public rollback
  selector, external-runtime discovery and invocation, and differential lane
  are removed. Records outside the current A3S OCI schema are unsupported and
  must be drained before upgrade; no compatibility decoder or alternate path
  recognition remains.
- **Real configuration qualification.** The Box Linux integration gate runs
  the pinned runtime's native private/host/donor network, read-write/read-only
  volume, tmpfs, inline/file/direct initialization, failure, and cleanup
  matrix before exercising the Rust, Python, TypeScript, and Go SDK lifecycle.
- **Machine bridge protocol v2.** Python and TypeScript share one complete
  capability handshake across concurrent first calls, while Go rejects
  duplicate capabilities during client construction. The native packages fail
  closed on malformed typed values and standard Base64, expose the effective
  `microvm` or `sandbox` isolation returned by the runtime, and use stable
  `bridge_timeout`, `binary_not_found`, and protocol error categories.

### Fixed

- **Generation and isolation fidelity.** Stale generation requests return
  `conflict`; command and filesystem bridge operations reconnect through the
  persisted execution record instead of assuming MicroVM isolation; Go and
  TypeScript reject invalid Sandbox identity, generation, state, isolation, or
  an isolation change in a lifecycle response.
- **Poisoned build-cache recovery.** Layer cache hits verify the regular-file
  type, recorded size, and SHA-256 content before reuse. A later valid store
  repairs a corrupt content-addressed blob instead of repeatedly serving it.
- **Reliable Cloud Sandbox completion.** Managed Sandbox inspection retains an
  A3S OCI generation until its exact exit status is available, and the default
  seccomp profile permits namespaced System V shared-memory operations needed
  by PostgreSQL while retaining its default-deny host-control boundary.
- **Deterministic termination and filesystem diffs.** Managed kill operations
  retain the backend's authoritative signal exit status and derive the
  standard `128 + signal` result when an abruptly killed shim cannot be reaped.
  One-shot commands that finish during readiness keep their exact status and
  output for foreground draining, while the runtime installs a
  first-writer-wins rootfs baseline before the workload can mutate the guest
  filesystem.
- **Linux bridge peer switching.** Named-network peer Ethernet frames are
  switched between libkrun guests by a bounded shim adapter while passt remains
  responsible for gateway traffic, published ports, and outbound connectivity.
  Peer ARP requests bypass passt's proxy ARP so guests cannot cache the gateway
  MAC for another box's address, and switch-facing TCP/UDP copies complete
  virtio checksum offload before entering the destination guest. Detached stop
  and remove paths reap passt before deleting its durable PID file.
- **Ubuntu passt sandbox startup.** Bridge startup now waits until passt has
  completed sandbox initialization and, only for an explicit unprivileged-userns
  denial under a root launcher, retries with root as passt's sandbox identity.
  Teardown also recognizes the packaged `passt.avx2` process name so optimized
  passt daemons cannot survive box removal.

## [3.1.0] — 2026-07-23

### Added

- **Fluent programmable CI/CD builders.** Rust, synchronous/asynchronous
  Python, and TypeScript can build OCI images, create named volumes and bridge
  networks, configure typed Sandboxes, and run stdin-backed scripts through the
  same local A3S Box runtime.
- **Complete local SDK resource management.** The native SDKs now cover image
  pull, inspect, tag, save, load, push, remove, and prune; volume and network
  lifecycle; Sandbox list/get; runtime diagnostics and disk usage; filesystem
  snapshot list/get; and deterministic cleanup.
- **Cross-capability soak validation plan.** Runtime, image, build, storage,
  networking, SDK, provider, TEE, Kubernetes, Windows, and upgrade behavior now
  share named rehearsal, guardrail, release, and endurance profiles with a
  common evidence contract and promotion gates.

### Changed

- **Local Sandbox lifecycle parity.** Rust, Python, and TypeScript handles now
  expose generation-fenced stop, restart, remove, logs, stats, and execution
  results while preserving zero-configuration local operation.
- **Versioned machine bridge contract.** Python and TypeScript validate a
  checked operation inventory and typed request/response values instead of
  parsing human CLI output.
- **Verified SDK publication.** Stable Python and TypeScript packages are
  published only from artifacts produced and tested by the matching successful
  GitHub Release workflow.

### Fixed

- **Windows release toolchain target.** Windows Actions install the Linux musl
  target required to build the bundled guest init alongside native WHPX
  binaries.
- **Persistent snapshot SDK coverage.** Cross-language tests verify persistent
  rootfs paths and lifecycle results across snapshot-backed local workflows.

## [3.0.12] — 2026-07-23

### Added

- **Zero-configuration local Rust, Python, and TypeScript SDKs.** Native
  `Sandbox`, commands, files, lifecycle, and a small Python Code Interpreter
  facade now share the direct Rust runtime implementation through a versioned
  structured bridge. Local use has no endpoint or API key, supports the
  default MicroVM and explicit shared-kernel Sandbox isolation levels, and the
  packages do not depend on, wrap, or re-export official E2B SDKs.
- **Runnable native Windows WHPX path.** Windows packages now include the
  `libkrunfw.dll` companion kernel alongside `krun.dll`, and the native path is
  documented and validated with Alpine foreground/detached workloads, separated
  output streams, structured logs, and real workload exit codes.
- **Windows WHPX soak runner.** `scripts/windows-whpx-soak.ps1` repeatedly runs
  the supported real lifecycle, storage, bind-mount, port, stats, and virtio-fs
  stress paths, retains per-test logs and a JSON summary, and fails when an
  iteration leaves an `a3s-box` or shim process behind.

### Changed

- **Explicit remote E2B compatibility boundary.** Network endpoint, domain, and
  API-key configuration is now limited to the self-hosted compatibility
  service and an opt-in remote configuration helper. Remote conformance uses
  unchanged official E2B clients; native local SDK tests are independent.
- **Explicit Windows CPU boundary.** Windows defaults to one vCPU and rejects
  unsupported SMP requests before image pull; Linux and macOS keep their
  existing two-vCPU default.
- **Reproducible native dependency releases.** The libkrun-sys crate now ships
  checksum-pinned Unix source and the exact tested Windows runtime in
  deterministic archives, stays below the crates.io size limit, and publishes
  license notices and matching libkrunfw/Linux corresponding source before the
  crate. Release actions and Cargo dependency resolution are immutable/pinned.

### Fixed

- **Bounded guest entrypoint transport.** Workload executable, arguments,
  working directory, user, and stdin mode are now carried in a validated,
  size-limited rootfs file instead of libkrun's bounded guest kernel command
  line, preventing long valid arguments from hanging WHPX boot.
- **WHPX soak launch directory.** The Windows soak runner now enters the Cargo
  workspace before its build phase, so the documented repository-root command
  works without `-SkipBuild`.
- **WHPX vCPU register access.** Hypervisor register buffers now satisfy WHPX
  alignment requirements, preventing the host-side crash seen during vCPU
  setup.
- **Windows result and rootfs handling.** The parent runtime collects completed
  guest stdout/stderr and exit status after libkrun terminates the shim, while
  Windows layer extraction recreates image symlinks instead of dropping them.
- **Windows persistence and guest paths.** Native cross-process locks now
  serialize cache, network, volume, and credential updates; cache metadata
  tolerates transient Windows sharing conflicts, and guest paths remain
  slash-normalized for Dockerignore, rootfs symlinks, and CLI diff output.
- **Windows live logs and repeated port forwarding.** Detached workloads now
  expose stdout and stderr while they are still running, and the guest drains
  coalesced control frames so a published TCP port accepts sequential
  connections instead of stalling after the first request.
- **Windows graceful stop and clean persistent capture.** Reattached CLI
  managers now deliver the configured stop signal over the WHPX host-control
  channel, wait for guest shutdown, and force-terminate an unresponsive shim
  without leaving orphan processes. Persistent shutdown gets a bounded metadata
  finalization window; the host validates and atomically publishes manifests
  that virtio-fs cannot rename, while runtime-owned logs remain outside commits
  and filesystem snapshots.
- **Windows virtio-fs POSIX metadata.** Guest `chmod`, `chown`, and umask-derived
  modes now remain visible for the VM lifetime, while Box terminal metadata
  capture and boot replay preserve them through stop, restart, and commit.
- **Windows soak lifecycle coverage.** Named-volume validation now advances a
  terminal managed execution with `restart`, and the unchanged 2,048-file,
  five-pass virtio-fs tar stress keeps its full workload with a separate WHPX
  timeout and per-pass progress markers.
- **Windows bind-mount parsing.** Drive-letter and UNC sources are classified as
  bind mounts and retain their Linux guest target through runtime preparation,
  including read-only single-file mounts.
- **Filesystem snapshot command restoration.** Starting a box restored from a
  filesystem snapshot now preserves its persisted command even when the
  restored layout has no OCI metadata.
- **OCI and cache path confinement.** Image layouts, registry pushes, rootfs
  metadata, layers, and the image store reject traversal, malformed digests,
  symlinks/reparse points, special files, oversized metadata, and descriptor
  size/hash mismatches before reading, copying, publishing, or deleting data.
- **Dockerfile RUN capture fencing.** Linux local builds execute RUN inside a
  private PID/mount namespace, while pool builds wait for lease release and VM
  destruction before capturing a layer; detached descendants can no longer
  mutate a supposedly completed layer or cache entry.
- **Windows snapshot link safety.** Snapshot copying classifies a link from its
  own no-follow metadata instead of following its target outside the source
  tree.

## [3.0.11] — 2026-07-19

### Added

- **A3S Runtime recovery fault fixtures.** The real-provider R17 Recovery
  profile now cancels a Task apply after its Sandbox is running and seeds a
  provider reservation without starting it. Exact retries must reattach to
  those original identities, finish the pending work, and leave one resource
  before complete removal. The opt-in test provisions bounded runner and Tokio
  worker stacks instead of depending on an external `RUST_MIN_STACK`.
- **A3S Runtime tmpfs conformance.** The Sandbox-backed Runtime provider now
  advertises bounded tmpfs mounts, preserves `ro`/`rw` intent through guest and
  OCI paths, rejects protected destinations and unsupported mount kinds before
  mutation, and passes the real-provider R17 Mounts profile for read-only
  enforcement, restart isolation, and complete removal.
- **Pure deterministic Compose normalization.** Canonical ACL and bounded YAML
  inputs now produce one typed, byte-stable model covered by shared golden
  fixtures. A closed schema reports unsupported fields and values through
  stable codes and JSON Pointer-style paths. Stateless
  `ComposeRuntimePlan` translation is separate from CLI lifecycle state and
  Cloud desired state, and declared service network aliases now reach Runtime
  DNS endpoint registration.

### Fixed

- **A3S Runtime recovery and certification stability.** Terminal Sandbox
  owners and recovered log workers are reaped with PID identity fencing,
  naturally exited in-process owners are reclaimed, exec reserves time to
  return replayable timeout results, and structured log timestamps remain
  ordered across concurrent writers and host clock regression. The R17 suite
  now reports case-level exec failures and validates the exact bootstrap versus
  workload capability boundary.
- **Resilient and observable registry blob pulls.** Configuration and layer
  transfers now use bounded capped-exponential retries, exact HTTP Range resume,
  configurable no-progress deadlines, and bounded concurrent layer downloads.
  Structured progress reports actual bytes, attempts, and retry delays. Before
  atomic publication, declared size and SHA-256 remain mandatory; verified
  blobs can be reused across indexed image layouts through safe reflink/copy
  staging, while same-size corrupt candidates are rejected and downloaded.
- **Multi-platform OCI archive loading.** `load` now resolves direct and nested
  OCI or Docker image indexes to an explicit `--platform`, defaulting to Linux
  on the host architecture. It verifies declared sizes and SHA-256 digests for
  the selected index path, manifest, config, and layers, validates the config
  platform, and proves the normalized layout is consumable before publishing
  the tag.

## [3.0.10] — 2026-07-17

### Added

- **Opt-in shared-kernel OCI Sandbox execution.** Linux operators can select
  `--isolation sandbox` to run workloads through a certified OCI runtime with
  namespaces, seccomp, capabilities, `no_new_privs`, and cgroup v2. The
  hardware-backed MicroVM remains the default, and Box never silently falls
  back to the lower-isolation backend.
- **E2B-compatible service and native SDK release assets.** The ACL-configured
  control and TLS data planes now provide durable lifecycle, authenticated
  routing, current metrics, Filesystem, Process, stdin, PTY, and Python Code
  Interpreter contexts. Pinned official clients and the A3S Python sync/async
  and TypeScript packages pass the same real-Sandbox production matrix; native
  packages connect with `A3S_BOX_*` and do not require `E2B_API_URL`.
- **Owner-scoped E2B filesystem Snapshots.** The compatibility service now
  provides durable capture, source-filtered listing, restore, and delete with
  startup reconciliation, generation-fenced source quiescing, copy-on-write
  restores, resolved OCI-default fidelity, Unix ownership/mode preservation,
  and in-use deletion conflicts. Official and A3S Python sync/async and
  TypeScript clients pass the same real-runtime matrix on A3S OS.
- **Owner-scoped E2B Volumes.** The compatibility service now provides durable
  create, connect, list, and delete operations plus authenticated
  volume-content directory, file, path, and metadata routes, with startup
  reconciliation for interrupted transitions. Official and A3S Python
  sync/async and TypeScript clients prove bidirectional Sandbox mounts, UID/GID
  mapping, in-use deletion conflicts, and cleanup against real Sandbox
  executions on A3S OS.
- **Runtime-backed E2B Sandbox logs.** The compatibility service now exposes
  generation-fenced v1 and v2 Sandbox log routes over the canonical structured
  runtime logs, including cursor, direction, level, search, and limit
  semantics. Rotated gzip files are read oldest-first with decompression
  bounds, live partial tails are ignored safely, and responses are stably
  ordered by timestamp across concurrent stdout/stderr writers.
- **Memory-preserving E2B Sandbox pause and resume.** The compatibility service
  now exposes generation-fenced pause/resume transitions backed by a certified
  shared-kernel runtime, preserves paused state across listing and
  reconciliation, and resumes
  through `connect` without shortening the existing TTL. Official and A3S
  Python sync/async and TypeScript clients prove that an already-running
  process survives the cycle. Filesystem-only pause remains explicitly
  unsupported.

### Changed

- **Canonical durable execution lifecycle.** CLI create, start, run, and
  restart paths plus the Rust SDK now share the generation-fenced managed
  execution manager, complete caller policy, crash-recoverable transitions,
  startup reconciliation, and terminal resource cleanup.

### Fixed

- **Sandbox runtime hardening.** Runtime and shim operations now fence process
  and cgroup identity, clean detached and failed executions, preserve split
  structured logs and rootfs state across cache transitions, tolerate
  restrictive service umasks, and emit runnable seccomp architecture data.
- **Runtime envd and OCI correctness.** Readiness is fail-closed; command
  sessions inherit the initialized environment and user home; resolver and
  runtime-managed file modes remain usable; managed pulls load credentials,
  retry Basic authentication, and replace conflicting hardlink destinations
  safely.
- **Legacy filesystem Snapshot restore fails closed.** Snapshot records from
  older builds that lack resolved OCI image defaults remain listable,
  inspectable, and deletable, but restore is rejected before execution
  reservation because the historical entrypoint, environment, user, and
  working directory cannot be reconstructed safely.
- **E2B Sandbox timeout starts at readiness.** A cold-starting Sandbox now
  receives its complete requested usable lifetime after both the runtime and
  envd control path are ready. Startup recovery applies the same rule, while
  preserving reconciliation of historical records that are already expired.

## [3.0.9] — 2026-07-11

### Added

- **macOS fault-injection endurance runner.** A new isolated Apple Silicon/HVF
  harness supports staged 2-hour, 24-hour, and 72-hour soak validation with
  shim/CLI termination, recovery assertions, resource sampling, admission
  gates, and machine-readable evidence.

### Changed

- **Native Node.js 24 GitHub Actions.** Checkout and artifact actions now use
  their native Node.js 24 releases, removing deprecation warnings from CI and
  release workflows.
- **Faster and more predictable runtime paths.** Package-cache preparation,
  warm-pool routing, bounded `info`, and macOS BuildKit VM execution have been
  tightened for repeated development and CI workloads.

### Fixed

- **Runtime correctness across lifecycle, networking, and storage.** Fixes
  include detached health scheduling, Compose variable defaults, quoted build
  arguments, commit metadata preservation, bridge peer and published Redis data
  paths, case-sensitive APFS rootfs handling, and virtiofs descriptor lifetime.
- **Cross-platform builds.** OCI metadata and warm-pool clients now compile on
  Windows, with Unix-only commit and health paths correctly gated.
- **Release automation.** The libkrun publish workflow is valid YAML again and
  no longer creates failed zero-job runs on every push.

## [3.0.8] — 2026-07-09

### Changed

- **Release automation temporarily skips Windows.** GitHub Actions releases now
  publish Linux x86_64, Linux arm64, and macOS arm64 artifacts without waiting
  for the Windows WHPX runner or triggering winget publishing.

## [3.0.7] — 2026-07-09

### Fixed

- **SDK crates.io publishing metadata.** `a3s-box-sdk` now declares crates.io
  version requirements for its internal Box dependencies, allowing release
  automation to publish the SDK crate.
- **Winget release automation clarity.** The winget workflow now uses the
  requested release tag for workflow-dispatch runs and reports a non-blocking
  first-submission warning when `A3SLab.Box` has not yet been added to
  `microsoft/winget-pkgs`.

## [3.0.6] — 2026-07-09

### Added

- **BuildKit VM backend for macOS Dockerfile `RUN`.** `a3s-box build` now
  supports `--builder auto|host|buildkit-vm`; on macOS, Dockerfiles containing
  `RUN` automatically delegate to BuildKit inside an A3S Linux VM unless the
  unsafe host-run escape hatch is explicitly enabled. The BuildKit VM backend can
  load OCI output back into the A3S image store or push directly with
  `--push --plain-http`.
- **Large workspace verification profile.** `a3s-box run` now supports
  `--package-cache pnpm|npm` and per-run `--virtiofs-cache`, with documented
  pnpm/npm cache, tmpfs, and virtio-fs settings for package-manager-heavy
  release checks.

### Changed

- **Faster cached rootfs copies on APFS.** macOS rootfs copy fallback now prefers
  copy-on-write cloning before byte-copying, reducing startup cost for
  short-lived cached-image boxes.
- **Nested runtime readiness inside guests.** Guest init prepares cgroup v2
  earlier so BuildKit/runc can start build containers inside the helper VM.

### Fixed

- **macOS release builds no longer require unsafe host `RUN`.** Dockerfile builds
  with `RUN` now have a supported isolated local path on Apple Silicon, including
  `linux/amd64` BuildKit builds.

## [3.0.5] — 2026-07-08

### Added

- **Explicit plain-HTTP registry push.** `a3s-box push` now supports
  `--plain-http`, `--insecure`, and Docker-compatible `--tls-verify=false` for
  trusted private registries. The Rust SDK exposes the same protocol selection
  through `RegistryProtocol` and `PushImage::plain_http(true)`.
- **CI-safe foreground runs.** `a3s-box run` now closes guest stdin by default,
  accepts `--no-stdin` for explicit non-interactive runs, and adds
  `--timeout <seconds>` for foreground commands. Timed-out runs stop/remove the
  box through the normal cleanup path and return exit code 124.

### Changed

- **Exec readiness waits are bounded and diagnosable.** Boot-time exec-server
  readiness probing now defaults to a 15s safety cap, logs progress with the
  socket path, exits early when the guest has already persisted an exit code,
  and can be tuned with `A3S_EXEC_READY_TIMEOUT_MS`.
- **More useful pnpm package caches.** `--package-cache pnpm` now also persists
  Corepack, `PNPM_HOME`, and npm cache data, disables Corepack's download prompt,
  and prefers offline package resolution by default. `a3s-box info` reports the
  pnpm cache volume status and size.
- **Stable host-volume traversal.** Guest virtio-fs mounts default to
  `cache=none` for safer large host tree traversal on macOS/HVF. Set
  `A3S_VIRTIOFS_CACHE=auto`, `always`, or `default` to override.

### Fixed

- **Rootfs writes through `/etc` symlinks.** Rootfs setup now writes generated
  files such as `/etc/nsswitch.conf` inside the guest rootfs even when `/etc` is
  an absolute symlink, fixing images such as `quay.io/skopeo/stable`.
- **Dockerfile build blockers.** Linux `RUN` now honors `WORKDIR` inside the
  chroot, declared `ARG` values are visible to `RUN`, unsafe macOS host-run
  propagates the build environment, `RUN chown`-only changes produce a layer,
  cached layers are copied into the active build directory before export, and
  layer-copy errors include the missing source/destination context.
- **Layer extraction directory-to-symlink replacements.** OCI layer extraction
  now prepares symlink destinations so a later layer can replace an existing
  directory with a symlink without failing.

## [3.0.4] — 2026-07-08

### Added

- **pnpm install benchmark parity.** `bench/bench.sh pnpm` and `just bench-pnpm`
  now benchmark a real project or the reduced `bench/fixtures/pnpm` fixture,
  split install time into VM boot, Corepack/pnpm setup, `pnpm fetch`, offline
  `node_modules` materialization on the project mount, tmpfs materialization,
  and full frozen install. When Docker is available, the harness also reports
  cold/hot Docker baselines and A3S/Docker ratios.

### Fixed

- **pnpm package-cache toolchain reuse.** `--package-cache pnpm` now persists
  Corepack's prepared pnpm toolchain with `COREPACK_HOME=/a3s-cache/pnpm/corepack`
  in addition to the pnpm store, avoiding repeated toolchain downloads across
  throwaway boxes.

## [3.0.2] — 2026-07-07

### Fixed

- **Dockerfile BuildKit cache mounts.** `a3s-box build` now parses
  `RUN --mount=type=cache,target=... <command>` instead of passing the
  `--mount` flag to `/bin/sh`, and fails clearly for unsupported mount types.
- **Foreground run lifecycle.** `a3s-box run --rm` now observes persisted guest
  exit codes and handles `SIGTERM` the same cleanup path as Ctrl-C, preventing
  interrupted foreground runs from leaving active box records behind.
- **OCI entrypoint resolution.** Relative image entrypoints such as
  `docker-entrypoint.sh` are resolved through the container `PATH`, matching
  common Docker image behavior.
- **Image store state errors.** Image index write/lock failures now include the
  affected path and an `A3S_HOME` hint so restricted environments can point Box
  at a writable state directory.

## [3.0.0] — 2026-07-06

### Added

- **Programmable-CI pipeline: parallel fan-out + typed JSON report (`a3s-box-sdk`).**
  `Base::run_parallel(steps, max_concurrency)` runs steps concurrently as isolated
  copy-on-write MicroVM forks (bounded, collect-all, results in input order) and returns a
  `Report` with a dependency-free `to_json()`. `StepResult` now carries separated
  `stdout`/`stderr`, `duration_ms`, and `metrics` parsed from `::metric <key>=<value>`
  guest-stdout lines (a machine-readable scoring channel for matrix/selection workloads).
  Steps run via `&self` (atomic fork counter), so fan-out no longer needs hand-rolled
  threads. The base auto-removes its snapshot on `Drop` (`--force`), and each fork is
  removed on every path (including panic). Box/snapshot names now carry per-process +
  per-instance entropy, so concurrent pipelines from the same image+setup can no longer
  collide and tear down each other's boxes. A fork that hits a *transient*
  infrastructure failure (restore/start/boot) is retried — `WarmBase::infra_retries`,
  default 2 — since its command never ran, which keeps sustained high-concurrency
  churn green. Validated end-to-end on a real `/dev/kvm` host.
- **Crash-orphan recovery + real-VM integration & soak tests.** `sweep_orphans()`
  reclaims `ci-base-*` boxes/snapshots left behind when a pipeline process is
  `SIGKILL`ed / OOM-killed (its RAII cleanup never runs), by matching the dead owner
  pid embedded in the resource name — and it never touches a live peer's resources.
  Added `#[ignore]`'d real-microVM integration tests (`tests/integration_kvm.rs`:
  warm + fork-per-step, cache, parallel order/metrics, fork isolation, leak-freeness,
  sweep) and a soak test (`tests/soak_kvm.rs`: sustained fork-eval churn stays
  leak-free and RSS-stable), both wired into the KVM CI gate.
- **`a3s-box-ci` runner + `warm_base` retry.** A dependency-free `a3s-box-ci` binary
  (shipped by the `a3s-box-sdk` crate) bridges any agent/tool to the pipeline: a
  line-based spec on stdin → a JSON `Report` on stdout (`a3s-box-ci run -`), plus
  `a3s-box-ci sweep` for crash-orphan recovery. `warm_base` now also retries on a
  transient infrastructure failure (sharing the step-fork's `retry_infra` budget),
  so concurrent same-image warms are more robust under load.

### Changed

- **`StepResult.logs` is replaced by separated `stdout` / `stderr` fields**
  (use `StepResult::combined()` for the old concatenated view). Breaking for
  direct `.logs` field access on the `a3s-box-sdk` pipeline API.

### Fixed

- **Concurrent same-image pipelines could corrupt each other's rootfs cache.**
  `RootfsCache::prune` (run after a cache-miss `put`) evicted least-recently-used
  entries with no in-use guard, so it could `remove_dir_all` a cache entry that
  another box was simultaneously using as its overlayfs **lowerdir** — the peer's
  `mount(2)` then failed with `No such file or directory (os error 2)`, and the
  failure persisted through retries (the backing was gone). Added the same in-use
  guard `SnapshotStore::prune` already applies to live copy-on-write lowers: each
  overlay box records the cache key it holds in a `<box_dir>/.rootfs-cache-key`
  marker (removed with the box dir), and `prune` skips any still-referenced key.
  Found via a concurrent-pipeline chaos test driven through a3s-code; root-caused
  and verified on a real `/dev/kvm` host (the concurrency scenario went from ~50%
  failure to reliably green).

## [2.6.0] — 2026-06-26

### Added

- **`containerd-shim-a3s-box-v2` — Kubernetes RuntimeClass integration.** A new
  containerd runtime-v2 shim (standalone `containerd-shim/` crate) that lets a vanilla
  Kubernetes cluster route `runtimeClassName: a3s-box` pods to the a3s-box MicroVM
  runtime via a containerd runtime handler, without replacing the node CRI. It maps the
  containerd Task API onto the `a3s-box` CLI (pod sandbox → placeholder; workload →
  detached MicroVM; `kubectl exec` → `a3s-box exec`). Deploy manifests under
  `deploy/shim/` (RuntimeClass, additive containerd config, example pod). Validated on a
  real `/dev/kvm` Kubernetes node: a `runtimeClassName: a3s-box` pod reaches Running on
  a real libkrun MicroVM. Still experimental — `kubectl exec`/log streaming depend on the
  guest exec control channel and are not yet fully validated; single-container,
  TSI-networked pods are the supported shape.

### Fixed

- **VMM shim now survives teardown of its launcher's session.** `VmController` puts the
  libkrun shim in its own session (`setsid` via `pre_exec`) so a process-group/cgroup
  kill of a foreground launcher (e.g. a containerd-shim `a3s-box run`) no longer reaps
  the shim and removes the box's `exec.sock`, which previously caused `a3s-box exec` to
  fail with "exec socket missing".

### Changed

- **`a3s-libkrun-sys` build downloads are resilient.** The libkrunfw fetch now retries and
  aborts stalled transfers (`curl --retry --speed-limit/--speed-time`) instead of a bare,
  unbounded `curl` that could hang forever on a flaky network.

## [2.5.2] — 2026-06-22

### Changed

- **`a3s-box-sdk` pipeline: faster per-step readiness wait.** `pipeline::wait_ready`
  now polls with exponential backoff (25ms → … → 500ms cap, ~30s budget) instead of a
  fixed 500ms sleep, so a step's box is detected ready in ~100-200ms instead of ~500ms —
  cutting noticeable latency from multi-step pipelines. No API change.

## [2.5.1] — 2026-06-22

SDK crate naming. No runtime behavior change.

### Changed

- **`a3s-box-sdk` is now the general-purpose Rust SDK.** The programmable-CI pipeline
  API (added in 2.5.0 as the `a3s-box-ci` crate) is now `a3s-box-sdk`, under the
  `a3s_box_sdk::pipeline` module, so the SDK can grow beyond CI. The error type
  `CiError` is now `pipeline::PipelineError`. **`a3s-box-sdk` is published to crates.io.**
- **The former `a3s-box-sdk` (MicroVM workload-execution SDK for a3s-lambda) is renamed
  to `a3s-box-lambda`.** Consumers (e.g. a3s-lambda) must update `use a3s_box_sdk::…` →
  `use a3s_box_lambda::…`. It remains unpublished (path-only deps).

## [2.5.0] — 2026-06-22

Programmable CI on a3s-box: copy-on-write snapshot restore — fork a warmed snapshot as
a near-instant overlay mount instead of a full rootfs copy — plus a new, dependency-free
Rust SDK crate (`a3s-box-ci`) for writing CI pipelines as code, each step in its own
MicroVM. No breaking API changes.

### Added

- **`a3s-box-ci` — programmable CI pipeline SDK.** A pipeline is a Rust program; box is
  the execution backend (one kernel per step, exit code = pass/fail). `warm_base`
  snapshots a warmed base once, `Base::step` forks it per step, and a content-addressed
  `FileCache` skips unchanged steps. A thin, zero-dependency wrapper over the `a3s-box`
  CLI; the DAG is the caller's code (no YAML, no engine).

### Changed

- **Snapshot restore is now copy-on-write.** `a3s-box snapshot restore` no longer
  deep-copies the snapshot's rootfs into the new box. It writes a `.snapshot-lower`
  marker and the runtime mounts the snapshot's pristine stored rootfs as a
  read-only overlay lower with a fresh per-box upper. Forking a warmed snapshot is
  now a near-instant overlay mount instead of a full rootfs copy: forks share one
  read-only lower, each writes to its own isolated upper, and the snapshot stays
  pristine — making snapshot-per-step CI fan-out cheap (measured on KVM: a fork's
  upper was 5.3 MB vs the 14 MB rootfs). Falls back to a full copy on a non-overlay
  host via the CopyProvider; boxes already restored via the old `.snapshot-rootfs`
  copy path keep booting unchanged.
- **`snapshot rm`/`prune` never delete a snapshot still in use.** Because a restored
  box now shares the snapshot's rootfs as its copy-on-write overlay lower, deleting
  that snapshot would break a live overlay or stop the box from re-starting. `rm`
  checks every box's `.snapshot-lower` marker and refuses (non-zero exit) while any
  box references the snapshot, naming them (`--force` overrides); `snapshot prune`
  and auto-prune-on-create skip in-use snapshots when evicting.

## [2.4.0] — 2026-06-17

Post-2.3.0 hardening: three adversarial audits — production-operability (24
findings), untrusted-input security (4), and concurrency/atomicity (4) — all
fixed and validated on real microVMs (composed-main real-VM CI Integration, a
2-hour / 4584-op endurance soak with zero leak, and complex stateful containers:
volume persistence, a stateful database across restart, and a web server). No
breaking API changes.

### Security

Image extraction runs **host-side before the microVM boots**, so a malicious
image's reach here bypasses VM isolation:

- **Arbitrary host file write via registry digest path-traversal (CRITICAL).** The
  manifest digest (`Docker-Content-Digest`, returned verbatim by the registry)
  flowed into `Path::join` unvalidated, so `sha256:../../../../<path>` wrote the
  attacker-shaped manifest to an arbitrary host path on `pull` in the default
  config (signature policy is Skip by default; the box runtime often runs as
  root). Digests are now validated as canonical `sha256:<64-hex>` at the trust
  boundary before any path use.
- **Arbitrary host file/dir deletion via whiteout symlink escape.** A layer
  whiteout whose parent was an absolute symlink (e.g. `esc -> /etc`) deleted host
  files/dirs through it. Whiteout parents are now confined within the extraction
  target.
- **Host disk exhaustion via decompression bomb.** Layer pull and build
  `ADD`/`COPY` auto-extract streamed gzip/zstd/bzip2/xz with no decompressed-size
  cap. Bounded by `A3S_BOX_MAX_LAYER_BYTES` (16 GiB) and
  `A3S_BOX_MAX_BUILD_EXTRACT_BYTES` (4 GiB), env-overridable.
- **CRI seccomp `localhostProfile` path confinement.** The attacker-set pod field
  was read off disk unconfined (an arbitrary host-file open oracle); it is now
  confined to `A3S_BOX_SECCOMP_PROFILE_ROOT` (default `/var/lib/kubelet/seccomp`),
  rejecting `..` and out-of-root paths.

### Fixed

- **Daemonless lifecycle concurrency races** (the `monitor` daemon, CLI
  processes, and CRI server coordinate via a per-write flock that does not span an
  `await`):
  - The monitor no longer resurrects a box the user `stop`ped during its
    up-to-10s health-restart window.
  - A user `restart` and the monitor's auto-restart can no longer both boot the
    same box (now serialized by a per-box boot lock); previously the second record
    write overwrote the first's PID, orphaning an untracked VM.
  - `kill`'s host-signal fallback re-checks PID start-time identity before
    signalling, so a reused PID is never signalled.
  - The warm pool no longer leaks a VM pushed into the idle set during shutdown
    drain.
- **Operability (24 findings)** across crash-recovery, upgrade-compat,
  disk-pressure, concurrency, network-lifecycle, and config-validation — e.g.
  PID-reuse liveness via start-time identity, corrupt-store quarantine instead of
  a hard fail, durable (fsync'd) state writes, bounded snapshot / build-cache /
  CRI-log growth, atomic CRI network attach, stable bridge IPs across stop/start,
  and fail-closed `--cpus` / `--memory-swap` validation.

### Changed

- New operator-tunable caps (generous defaults, env-overridable), documented in
  the Environment variables table: `A3S_BOX_MAX_LAYER_BYTES`,
  `A3S_BOX_MAX_BUILD_EXTRACT_BYTES`, `A3S_BOX_SECCOMP_PROFILE_ROOT`,
  `A3S_BOX_MAX_SNAPSHOTS` / `A3S_BOX_MAX_SNAPSHOT_BYTES`.

## [2.3.0] — 2026-06-16

A security and hardening release closing a 35-finding adversarial audit (plus
new finds): both criticals and every security / data-loss / DoS / resource-leak
/ hang finding is fixed. The headline isolation and resource-enforcement fixes
were validated on real microVMs (measured CPU throttling, in-guest cgroup
limits, and TTY confinement), not just CI. No breaking API changes; behavior
changes are noted below (resource limits and TTY security controls that were
silently ignored are now actually enforced).

### Security

- **TTY containers were unconfined** — a CRI `tty: true` workload ran through the
  PTY path, which applied **none** of the pod's securityContext: full
  capabilities, no seccomp filter, `no_new_privs` unset, no cgroup, and no
  masked/readonly path restrictions. The PTY path now performs the **same**
  confinement + container setup as the exec path (seccomp, capability drop/keep,
  no_new_privs, supplemental groups, per-container cgroup, `/proc`+`/dev`, and
  MaskedPaths/ReadonlyPaths/readOnlyRootFilesystem). Real-VM verified.
- **TEE/attestation** — RA-TLS now verifies the TLS CertificateVerify signature
  (proof-of-possession), defeating captured-certificate replay; sealed-storage
  rollback protection binds the version into the AEAD so a forged version fails
  authentication; an empty SNP certificate chain fails closed; container env
  secrets are no longer written to debug logs.
- **OCI build** — COPY/ADD source and destination paths are contained against
  traversal escapes; ADD-from-URL is bounded.

### Fixed

- **Resource limits (cgroup) now actually enforced** — `--cpu-quota`/`--cpu-period`/
  `--cpu-shares`, `--pids-limit`, `--memory-reservation`, and `--memory-swap` are
  plumbed to and applied by the in-guest per-container cgroup on the run, CRI,
  deferred-main (warm-pool), and TTY paths; the dead/redundant host-side cgroup
  path (which never enforced anything and leaked an empty cgroup) was removed;
  `container update` no longer writes to the root cgroup when the per-container
  slice can't be resolved.
- **CRI lifecycle** — `StartContainer` claims the Created→Running transition
  before spawning the workload (no concurrent double-spawn); `RunPodSandbox`
  tears down the booted microVM + network if the request is cancelled before the
  sandbox is registered (was an orphaned-VM leak); mountinfo octal escapes are
  decoded before unmount (prevented host-data-loss `remove_dir_all`); a wedged
  guest can no longer hang the host exec/stop path.
- **Cross-process data-loss races** — IP allocation, volumes, the image index,
  and `credentials.json` are guarded by cross-process locks with load-fresh RMW,
  closing duplicate-IP / lost-pull / lost-login / lost-volume races.
- **Resource leaks** — a failed VM stop, a box `rm` mid-restart, and a partial
  `create`/`snapshot restore` no longer leak the VM / overlay / box directory;
  the host per-box cgroup dir is reclaimed on teardown.
- **Robustness** — checked integer math in the CLI size/memory parsers (no panic
  on a fat-fingered flag); atomic + idempotent layer/rootfs cache writes (no
  concurrent-build corruption); bounded `console.log` for every log driver
  (no disk-fill); snapshot metadata tolerates missing fields and surfaces a
  warning instead of silently dropping a snapshot.

## [2.2.0] — 2026-06-15

A correctness and hardening release: 24 fixes across the CLI state machine,
runtime resource limits, guest-init I/O, the OCI store, networking, the warm
pool, and the CRI server. No breaking changes. CRI conformance re-verified with
zero regression (see below).

### Added

- **Health-check TSI warning**: `run` now warns when a health check probes
  `localhost` under TSI networking, where the probe cannot reach the guest.

### Fixed

- **CLI state machine** — route every status-update command (`stop`, `start`,
  `kill`, `pause`, `unpause`, `rename`, `restart`) through the atomic
  `StateFile` primitives, closing a load-modify-save TOCTOU that could clobber
  concurrent box state; make box registration atomic in `compose` + `snapshot`
  (orphan-VM race) and unmount the overlay before deleting the box directory in
  `compose` cleanup.
- **Resource limits** — enforce `--pids-limit` on the `run` path via an in-guest
  cgroup `pids.max`; harden `resize` by rejecting shell-injectable cpuset strings
  and clamping `cpu.weight` to its valid range.
- **guest-init** — retry the stdio relay on `EINTR` so container output is never
  truncated; fix a cgroup-mount TOCTOU, an stdio fd-leak, and a signal-64 edge;
  make container `stdout`/`stderr` re-openable by path (`/dev/stdout`,
  `/proc/self/fd/N`) so apps that reopen their logs (e.g. Apache httpd) start.
- **exec** — base64-encode exec args/env so shell quotes survive libkrun's
  environment passing.
- **OCI store** — refuse to store blobs whose digest algorithm cannot be
  verified; make store and build-cache writes atomic (stage + rename) so
  concurrent pulls/builds cannot corrupt a layer.
- **rootfs & networking** — overlay comma-guard + bounded unmount-retry in
  provider cleanup; stage single-file bind mounts so directory-sharing virtio-fs
  can serve them; kill passt on a boot-failure timeout, guard `terminate`
  against PID reuse, and reap passt on boot failure so the published port is
  released.
- **Warm pool** — fall back to cold boot when snapshot-fork is unavailable,
  instead of failing the pool fill.
- **CRI** — maintain the `StopPodSandbox` state invariant and report stats
  correctly for non-running containers; close stdin and send a port-forward
  `CLOSE` frame on streaming error paths; reject empty image references in
  pull/status/remove; resolve the image reference in `RemoveImage` so
  `rmi <short-tag>` (e.g. `alpine:latest`) works; surface container log-file
  open failures instead of swallowing them.

### Verified

- **CRI conformance: 73 Passed / 7 Failed / 17 Skipped** (`critest` v1.30.1, skip
  portforward; 80 of 97 specs) on `main` — unchanged pass count, **zero
  regression** from the fixes above. The 7 remaining failures are all
  microVM-architectural (mount propagation, host namespaces, AppArmor-enforce,
  non-recursive readonly mounts), not code defects.

## [2.1.0] — 2026-06-13

### Added
- **Native snapshot-fork (Copy-on-Write microVM cloning).** A booted template
  microVM can be snapshotted and many forks restored from it, instead of cold
  booting each one. The snapshot captures file-backed guest RAM plus KVM vCPU
  and virtio device state; each fork maps the RAM file `MAP_PRIVATE` so it pays
  only for the pages it dirties. Driven by `KRUN_SNAPSHOT_MEM_FILE` /
  `KRUN_SNAPSHOT_SOCK` (capture) and `KRUN_RESTORE_FROM` (restore), or
  per-VM via `BoxConfig`/`InstanceSpec`. Verified on `/dev/kvm`: a single fork
  is ~4× faster than a cold boot (~450 ms → ~110 ms), 100 forks complete in
  under ~1 s (~8 ms amortized per VM, ~13 MB RSS each), and `exec` runs real
  commands over virtio-fs inside the restored guest.
- **Warm pool snapshot-fork fill** (`pool start --snapshot-fork`): the pool
  cold-boots one template, snapshots it, then restores the rest of the pool
  from that snapshot. Combined with concurrent (JoinSet) fill this cuts
  fill-to-8 from ~12.4 s to ~1.9 s. Off by default; opt in with the flag.
- **`prune` command** (`a3s-box prune`, alias `container-prune`): removes every
  created, stopped, and dead box in one call, mirroring `docker container
  prune`. Running and paused boxes are never touched. Requires `--force`.
- **Per-VM snapshot/restore config seam**: `BoxConfig` and `InstanceSpec` carry
  `snapshot_mem_file`, `snapshot_sock`, and `restore_from`, so snapshot/restore
  can be requested per box instead of only through process-global env vars
  (per-VM config takes precedence over the env).

### Fixed
- **Concurrent box registration is now atomic.** `run` registered boxes by
  loading the full state, mutating, and saving under lock; concurrent launches
  could lose updates and the reconcile pass was O(N²). Registration is now an
  atomic, reconcile-free append, and the later rollback paths un-register
  correctly. Verified by launching 100 boxes concurrently with zero lost
  records.
- **`pool status` no longer errors when no pool daemon is running** — it exits
  successfully and reports that nothing is running, matching Docker-style UX.
- **Restore readiness is faster and OCI-free.** A restored fork skips the OCI
  pull (the template's cached rootfs is reused) and uses a short crash-detection
  grace (250 ms fixed → 40 ms) tuned for the restore path.

## [2.0.7] — 2026-06-06

### Added
- **Container log stream tagging**: `logs` now distinguishes a container's
  stdout from stderr (Docker json-file `stream` field), via libkrun's 3-fd
  split virtio-console (guest stdout → `console.log`, stderr →
  `console.err.log`). Foreground `run`/`attach` send the container's stdout to
  the terminal's stdout and stderr to its stderr; `logs` routes stderr lines to
  its stderr.

### Fixed
- **Container logs are now complete and correct.** The log processor moved from
  the ephemeral launching CLI into the shim (the box's lifetime process), so a
  detached `run -d` box no longer truncates its logs when the CLI exits — this
  also gives `--timestamps` real per-line emission times. The processor tails
  `console.log` like `tail -f` (it previously stopped at the first EOF, dropping
  lines a container logged after a quiet period). Runtime internals (guest-init
  tracing → `/dev/kmsg`; libkrun's `init.krun:` preamble filtered) are kept out
  of container logs.
- A box without `--rm` now survives its stop like a Docker stopped container —
  it keeps its dir and logs (so `logs`/`start` work afterwards) until `rm`.
- Single-file bind mount (`-v /host/file:/container/file`) no longer clobbers
  the target's parent directory.
- A rebuilt or re-tagged image becomes a prunable `<none>` dangling image
  instead of silently orphaning its on-disk layout (a disk leak); `images`
  renders it as `<none> <none>`.
- `-p 0:<container>` / `-p 0` now resolves to a real free host port.
- Named `--user` / `exec -u` is resolved inside the guest; `inspect` returns a
  JSON array with a Docker-shaped `State`; image-management parity (`inspect`
  array, `rmi` by short id / `--force`, `commit --change`, `tag` validation);
  `volume rm` exit code + `volume inspect` schema; `cp` mode + large-file.

### Added
- Registry mirrors: `A3S_REGISTRY_MIRRORS=host=mirror,...` pulls image content
  from a configured mirror while preserving the canonical image identity in the
  store (e.g. fetch `registry.k8s.io`/`gcr.io` images via an accessible mirror).
- CRI `SecurityContext.no_new_privs`: the guest sets `PR_SET_NO_NEW_PRIVS`
  before exec, so a setuid/setgid or file-capability binary can no longer raise
  the container process's privileges (privileged containers opt out).
- CRI `SecurityContext.readonly_rootfs`: the guest remounts the container root
  read-only before exec (writes to `/` fail), while `/proc`, `/sys`, and inner
  mounts stay writable.
- CRI pod DNS config: a pod's `DNSConfig` (servers, searches, options) is
  captured on the sandbox and rendered into each container's `/etc/resolv.conf`
  (falling back to the default when unset).
- Image-defined supplemental groups: when a container runs as a specific user,
  the guest applies the groups that user belongs to per the image's `/etc/group`
  (runc-style initgroups) and defaults the primary gid to the user's
  `/etc/passwd` group when no `RunAsGroup` is set.
- `import` creates a single-layer image from a rootfs tarball (`.tar`/`.tar.gz`),
  with Dockerfile-style `--change` directives (CMD/ENTRYPOINT/ENV/WORKDIR/USER/
  EXPOSE/LABEL/VOLUME) and `--message` — matching `docker import`.
- `images --filter` supports `reference=<glob>` and `label=<key>[=<value>]`
  (repeatable; all must match), matching common `docker images --filter` usage.
- `build --target <stage>` builds only up to the named (or indexed) stage of a
  multi-stage build and emits that stage's image; later stages are not executed.
- `build --no-cache` disables the layer build cache so every layer is rebuilt.
- `inspect <name>` is now polymorphic: it resolves a container first, then falls
  back to an image (matching `docker inspect`), instead of only handling boxes.
- `ADD --chown=user[:group]` is now supported (was "not supported yet").
- COPY/ADD `--chown` now also resolves named users/groups from the rootfs
  `/etc/passwd`/`/etc/group`, not only numeric IDs.
- `.dockerignore` support: a context-root `.dockerignore` now excludes matching
  paths from `COPY`/`ADD` (comments, blank lines, `!` negation with last-match-
  wins, and `?`/`*`/`**` globs). Previously `COPY . /app` copied everything —
  `.git`, `node_modules`, `.env` secrets — into the image; those are now kept
  out, matching Docker. (Applies to the build context, not `COPY --from`.)
- Layer-level build cache (Docker/BuildKit-style): `a3s-box build` reuses
  previously built layers across builds via a rolling chain key over each
  instruction (and, for `COPY`/`ADD`, the content of the source files), so an
  unchanged prefix is reused and a changed instruction/input rebuilds from that
  layer on. Cached at `~/.a3s/buildcache`, size-capped (default 2 GiB,
  `A3S_BOX_BUILDCACHE_MAX_BYTES`; oldest evicted first), best-effort.
- CRI `ReopenContainerLog` flush boundary: log rotation now asks the guest to
  flush and drains every buffered output chunk into the old log file (stopping
  at a flush-ack marker added to the exec protocol) before reopening, so output
  produced before the rotation cannot leak into the new file.
- `network prune` removes all networks not used by at least one box, and
  `system prune` now reaps unused networks too (matching `docker network prune`
  and `docker system prune`). A network is kept while it has a live endpoint or
  any box record (running or stopped) references it; predefined `bridge`/`host`/
  `none` are never pruned.

### Security
- Host network/IPC namespaces are now rejected fail-closed: a pod or container
  requesting `HostNetwork`/`HostIpc` (or a host user namespace) —
  `NamespaceMode::NODE` — gets a clear `Unimplemented` error instead of being
  silently run fully isolated. A microVM-per-pod has no host network or IPC
  namespace inside the guest, so silently accepting gave the workload wrong
  (fail-open) semantics. `HostPID` is accepted (the pod's shared VM-wide PID
  namespace satisfies it), as are `POD`/`CONTAINER`.
- AppArmor: a requested Localhost profile (modern `apparmor` SecurityProfile or
  the deprecated `apparmor_profile` string) is now validated against the host's
  loaded profiles and the container is rejected when the profile is not loaded,
  instead of being silently ignored. The microVM cannot enforce an in-guest LSM
  profile, so a loaded profile is accepted with a warning that it is not
  enforced. Passes critest "should fail with an unloaded profile".
- Non-privileged containers are now restricted to the runtime default
  capability set (e.g. no `CAP_NET_ADMIN`/`CAP_SYS_ADMIN`), adjusted by the
  container's `add`/`drop` capabilities; privileged containers keep the full
  set. Previously every container ran as full-capability root, so a
  non-privileged container could perform privileged operations (e.g. create a
  network bridge). The guest applies an exact keep-set via `capset` + bounding
  drop before exec.

### Added
- Pod port reachability: a port mapping with only a container port now publishes
  it on the same host port (Docker/containerd style), and a default (TSI) pod
  that publishes ports reports `127.0.0.1` as its pod IP — TSI binds
  `0.0.0.0:<port>` and forwards to the guest, so `podIP:<containerPort>` is
  genuinely reachable from the node. Passes the port-mapping and
  multi-container networking conformance specs. (Single-node reachability via
  the node loopback; not a unique cluster-routable pod IP, and concurrent pods
  publishing the same port still contend for the host port.)
- Crash recovery: on startup the CRI reaps sandbox microVMs orphaned by a
  previous crash/SIGKILL — it kills the leftover `a3s-box-shim` (matched by the
  box id in its argv), unmounts its overlay, and removes its box directory —
  instead of leaking the VM, mount, and disk across restarts. A graceful
  shutdown already reaps VMs, so this is a no-op then.

### Added
- Compose `depends_on` supports `condition: service_completed_successfully`:
  a dependent waits for its dependency to run to completion (exit 0) before
  starting. Previously this condition was rejected at config time.

### Fixed
- Docker build/runtime parity (found via a 51-case real-Linux probe):
  - Compose services resolve each other by their bare service name (e.g.
    `getent hosts db`), not only by the `{project}-{service}` box name —
    matching Docker Compose service discovery. Network endpoints carry DNS
    aliases that are written into peers' `/etc/hosts`.
  - `COPY --chown` ownership is now honored at runtime. The layer tar headers
    were stamped correctly, but the rootfs the container saw collapsed to root
    (`stat` reported `0:0`): layer extraction did not set `preserve_ownerships`
    and the overlay/rootfs-cache copy carried content/permissions but not
    uid/gid. Both paths now restore the layer uid/gid (root only), so
    `COPY --chown=4242:4343` shows `4242:4343` and `--chown=nobody` shows
    `65534:65534`; non-root ownership baked into base images is preserved too.
  - A relative `--workdir` (e.g. `-w sub`) is accepted and resolved against the
    image WORKDIR (`/srv/app` + `sub` => `/srv/app/sub`), matching Docker;
    previously any non-absolute workdir was rejected.
  - Build-time variable expansion now matches Docker: a later `ENV` value and
    `WORKDIR` expand earlier `ENV`/`ARG` (e.g. `ENV APPDIR=/srv/app` then
    `WORKDIR $APPDIR`), instead of keeping the literal `$APPDIR`. An undeclared
    `--build-arg` (no matching `ARG`) is no longer substituted, and a global
    pre-FROM `ARG` is now in scope for every stage's `FROM` and body (so
    `FROM alpine:$BASETAG` in a later stage resolves).
  - `COPY`/`ADD` expand wildcard sources (`COPY *.conf /etc/`) against the
    build context instead of failing "source not found"; a glob matching
    nothing errors like Docker. Remote `ADD` URLs are never globbed.
  - `LABEL a=1 b=2 c=3` and `EXPOSE 80 443 8080/udp` on one line now parse every
    item (previously LABEL merged into one key and EXPOSE kept only the first
    port); bare EXPOSE ports normalize to `<port>/tcp`.
  - `HEALTHCHECK --interval=1m30s` (Go compound durations) is accepted instead of
    erroring "Invalid duration".
  - `MAINTAINER` is accepted as deprecated-but-valid (builds, recorded as a
    `maintainer` label) instead of failing the build.
  - `--env-file` values are kept verbatim after the first `=` (Docker preserves
    `PADDED=  x  `); previously the value was whitespace-trimmed.
  - Runtime bare `-e KEY` (no `=`) copies `KEY` from the host environment
    (Docker passthrough) instead of erroring; `--label`/`--log-opt` stay strict.
- Short-lived `run` no longer stalls ~10s before returning. A container that
  exits quickly (e.g. `run alpine -- echo hi`) made the VM halt and the shim
  become a zombie; the boot-readiness wait checked liveness with `kill(pid,0)`,
  which reports a zombie as alive, so it waited the full exec-heartbeat timeout
  (~10s, intermittently, depending on a boot race). Readiness now uses a
  zombie-aware liveness check (`/proc` state on Linux) and returns promptly
  (~1.7s). Also speeds up the monitor restarting fast-exiting containers.
- `run`/`create` health flags accept Docker-style duration strings:
  `--health-interval 30s`, `--health-timeout 1m`, `--health-start-period 10s`
  (and compounds like `1m30s`) instead of only a bare integer, which was
  rejected with "invalid digit found in string". A bare number still means
  seconds, so existing usage is unchanged. (The Dockerfile `HEALTHCHECK` and
  compose-YAML paths already parsed durations.)
- `RUN chmod` (mode-only changes) are now captured into the build layer, so
  the common `COPY script.sh` + `RUN chmod +x script.sh` makes the script
  executable in the image (previously the chmod was dropped and the script
  could not be run as the entrypoint).
- `--read-only` no longer crashes the container: a direct read-only remount of
  the virtio-fs root can fail with EBUSY, which was fatal to init. It now falls
  back to a bind-remount and, if that also fails, logs a warning and runs the
  container writable instead of killing it.
- Multi-variable `ENV KEY1=V1 KEY2=V2` (several pairs on one line) was parsed
  as a single variable swallowing the rest (`KEY1="V1 KEY2=V2"`), so only the
  first key got set and downstream `$KEY2` expanded empty. ENV now parses all
  pairs (quote-aware, so `KEY="a b" K2=c` stays two vars). Single and legacy
  `ENV KEY VALUE` forms are unchanged.
- Image `USER` (named or numeric) and `run --user` are now applied to the
  container MAIN process, by the guest init right before exec (setgroups +
  setgid + setuid, after PID 1 finishes its root-only setup), reusing the same
  resolver the exec path uses (names via the image /etc/passwd, image
  supplementary groups). Previously this went through the shim's libkrun
  set_uid, which dropped the guest PID 1 to that user and could not work at all:
  a named USER was silently skipped (ran as root) and a numeric one crashed the
  container. Now `USER appuser` runs the process as appuser.
- `save`/`load` now round-trip the image tag: `save` stamps the image reference
  into the OCI `index.json` `org.opencontainers.image.ref.name` annotation, so
  `load` restores the tag (e.g. `rt:9`) instead of importing the image untagged
  (by digest only). `load` already read the annotation; `save` never wrote it.
- Image references with a purely numeric tag and no registry (`redis:7`,
  `node:18`, `postgres:16`, `ubuntu:24`) were mis-parsed: the numeric tag was
  treated as a registry port and dropped, so the reference resolved to the
  `:latest` tag instead. A colon with no `/` is always a tag (a bare
  `registry:port` with no repository is not a valid reference), so numeric tags
  now parse correctly — affecting pull, run, and `images` display.
- `COPY`/`ADD` now preserve symlinks instead of following them: a copied symlink
  (e.g. a shared library `libfoo.so -> libfoo.so.1`, or any `node_modules`/
  `/usr/lib` link) was dereferenced into a duplicate regular file, losing the
  link and bloating the image. Symlinks (including symlink-to-dir and dangling
  links) are now stored as symlink layer entries, matching Docker.
- Multi-stage `COPY --from=<stage> /abs/path` (and any absolute COPY/ADD source)
  was broken: the absolute source was resolved against the host root instead of
  the source stage's rootfs (`Path::join` discards the base for an absolute
  argument), failing with "source not found". Absolute sources are now resolved
  relative to the context/stage, so multi-stage builds work.
- Multi-layer image corruption in `a3s-box build`: layer digest and size were
  computed before the gzip stream was flushed to disk (the tar builder owning
  the encoder was dropped only at function end), so every layer recorded the
  same digest — the hash of the partial 10-byte gzip header — and `size` 10.
  Manifests referenced one wrong digest for every layer and the content-addressed
  blob store collapsed all layers into the first; single-layer images happened
  to round-trip, hiding the bug. The encoder is now finished before hashing.
- Container `/dev` now contains the standard device nodes (`null`, `zero`,
  `full`, `random`, `urandom`, `tty`), created in the guest before the container
  starts. Workloads that need them — e.g. Apache httpd, which reads
  `/dev/urandom` to seed its RNG and otherwise aborts with `AH00141` — now run.
  Fixes the multi-container exec/log conformance specs.
- The container log file is now created eagerly at `StartContainer` (instead of
  lazily when the first output arrives), so a caller that opens the log
  immediately after start — e.g. `ReopenContainerLog`, or before the container
  has produced any output — finds it. Fixes the critest "reopening container
  log" conformance spec.
- CRI image identity now follows the digest, matching real runtimes:
  - `ListImages`/`ImageStatus` coalesce references by content digest, so an
    image with multiple tags appears once with all `repo_tags`.
  - `ImageStatus` resolves an image by exact reference, image id (digest), a
    `name@sha256:...` digest pin, or an unnormalized name (e.g. a tagless name
    defaulting to `:latest`).
  - `RemoveImage` accepts an image id (digest), not just a tag/reference.
  - `PullImage` returns the content digest as `image_ref`, so different tags of
    the same image dedupe to one image id.
  - An image pulled by digest (`repo@sha256:...`) is reported with that
    reference as a `repo_digest` and empty `repo_tags` (digest pins have no tag).
  - `ImageStatus`/`ListImages` surface the image's configured user as `uid`
    (numeric `uid`/`uid:gid`) or `username` (named user), from the OCI config.
  - `CreateContainer` resolves the image the same way (exact ref, digest id,
    `name@sha256:` pin, or unnormalized name), via a shared `ImageStore::resolve`
    — so a container referencing an image by an untagged name now starts.
  - The full critest Image Manager conformance suite now passes (7/7): public
    image pull/remove by tag, without tag, and by digest; image status across
    all reference kinds; non-empty uid/username; and the listImage image and
    repoTag counts.
- `stop` now stops containers gracefully and honors the image `STOPSIGNAL`. The
  CLI signalled the shim directly, but libkrun renames the shim and a host
  signal kills the VM abruptly, so the container never ran its stop handler —
  a `STOPSIGNAL SIGINT` image, or even a plain SIGTERM trap, was ignored. The
  stop signal is now delivered inside the guest over the exec channel (a
  `signal-main` control to the container's main process); the container runs its
  own shutdown and exits, then guest init exits and the VM halts cleanly. A
  container that ignores the signal is still force-killed at the stop timeout.

## [2.0.6] — 2026-06-01

### Added
- CRI Linux SecurityContext: `RunAsUser`/`RunAsGroup`/`RunAsUserName` (passwd
  lookup), `SupplementalGroups` (setgroups), `MaskedPaths`/`ReadonlyPaths`, and
  the `RuntimeDefault` seccomp profile (default BPF filter → `Seccomp: 2`).
- `/proc` and `/sys` are now mounted inside the container chroot, so in-container
  reads of `/proc/self/*` and `/sys/class/*` work like any container runtime.
- Pod sysctls: safe sysctls from `PodSandboxConfig` are applied in the guest at
  VM boot.
- Writable CRI volume mounts (materialized by copy into the rootfs; read-only
  and host-path-symlink volumes included).
- Graceful shutdown: on SIGTERM/SIGINT the CRI reaps every sandbox VM and
  unmounts its overlay, so microVMs/overlays no longer orphan across restarts.

### Fixed
- Corrected the CRI v1 `LinuxContainerSecurityContext` proto field numbers to the
  official spec (kubelet/critest can now decode security-context pods).
- `RemoveContainer` force-removes a running container (stops it first), per the
  CRI contract.
- Security & safety hardening from an adversarial code review (16 confirmed
  findings): a container image/pod env can no longer spoof the `A3S_SEC_*`
  security envelope (privilege escalation); the seccomp BPF filter is built
  before `fork` (no async-signal-unsafe allocation in the post-fork child —
  a musl malloc-deadlock risk); MaskedPaths/ReadonlyPaths mounts are idempotent
  (no per-exec mount leak); MaskedPaths/ReadonlyPaths/sysctl names are
  path-traversal validated; plus panic/leak/non-Linux-build fixes.
- `ReopenContainerLog` is now synchronous (waits for the supervisor to reopen the
  log) — correct CRI semantics for log rotation.

### Conformance
- `critest` v1.30.1: 44 of 82 runnable specs pass (up from 21), with no
  regressions. Remaining failures are environmental (registry egress),
  guest-kernel-limited (bridge/mqueue/AppArmor), architectural (mount
  propagation), or test-image artifacts — see `docs/cri-conformance.md`.

## [2.0.5] — 2026-05-31

### Added
- CRI `exec` works end to end over the Kubernetes SPDY/3.1 `remotecommand`
  protocol — `kubectl exec` / `crictl exec` (non-TTY and TTY), stdin, stdout,
  stderr, and exit-code propagation. Implemented in `cri/src/spdy.rs`; the two
  critest exec conformance specs now pass.

### Fixed
- CRI server is now reachable by standard gRPC clients (`crictl`, the kubelet,
  `critest`) over its Unix domain socket. `grpc-go >= 1.57` sends the
  percent-encoded socket path as the HTTP/2 `:authority`, which upstream `h2`
  rejected with a `PROTOCOL_ERROR` stream reset before any CRI RPC ran. A
  vendored `h2` patch (`third_party/h2`, wired via `[patch.crates-io]`) relaxes
  authority validation for UDS-style values; the full pod+container lifecycle
  (`runp`/`create`/`start`/`ps`/`stop`/`rm`/`stopp`/`rmp`) now works end to end.

### Changed
- Split the 7732-line `cri/src/runtime_service.rs` into a focused
  `runtime_service/` module (no behavior change).

## [2.0.4] — 2026-05-09

### Changed
- README and product documentation now describe the verified local CLI runtime,
  image lifecycle, networking, Compose subset, TEE boundaries, and experimental
  CRI surface without Docker/Kubernetes overclaiming.

## [0.8.12] — 2026-03-20

### Fixed
- macOS bridge networking restored for shim-hosted netproxy so `localhost` port publishing works reliably again
- Linux release CI restored by adding the missing `prometheus` dependency back to the workspace
- Windows release builds no longer fail on non-macOS network setup bindings
- Release workflow can dispatch the winget publish workflow with `actions: write`

## [0.4.0] — 2026-02-18

### Added
- Helm chart for Kubernetes deployment (`deploy/helm/a3s-box/`)
- Network isolation enforcement via `--isolation` flag on `network create`
- Image signature verification CLI flags (`--verify-key`, `--verify-issuer`, `--verify-identity`)
- Prometheus metrics auto-activated on every box boot
- Embedded shim support in SDK (`--features embed-shim`)
- Compose orchestration execution (`compose up/down/ps`)

### Changed
- CI workflow optimized: platform builds use `cargo check` instead of full release build
- Clippy and SDK checks now include stub libkrun for reliable linking
- README rewritten based on verified capabilities
- Shared CLI helpers extracted into `commands/common.rs` (DRY)
- Large files split into focused submodules
- Vendored a3s-transport replaced with a3s-common dependency

### Fixed
- Codesign race condition on macOS: concurrent tests no longer fail with file lock protection
- `build/` and `dist/` gitignore patterns scoped to root only

### Removed
- Root Dockerfile (legacy prototype, not part of Box)
- `.dockerignore` (no longer needed)
- `src/sdk/PLAN.md` (completed plan)
- Duplicate `deploy/daemonset.yaml` and `deploy/runtime-class.yaml`
- `deploy/examples/ai-agent-pod.yaml` (a3s-code specific, not Box)
- Kustomize manifests (replaced by Helm chart)
- Dead documentation links in README
- Dead code: `find_agent_binary`, agent/gRPC port 4088 code
- `updater` crate (moved to separate repo)

## [0.3.0] — 2025-02-17

### Added
- Python SDK (`pip install a3s-box`) — async API, streaming exec, file transfer (25 tests)
- TypeScript SDK (`npm install @a3s-lab/box`) — Node.js API, async iterator streaming (21 tests)
- Embedded Rust SDK — `BoxSdk` → `Sandbox` lifecycle, exec/PTY, streaming, file transfer, port forwarding, persistent workspaces, execution metrics (18 tests)
- Full release pipeline — crates.io, PyPI, npm, Homebrew, GitHub Release
- Kubernetes BoxAutoscaler CRD — ratio-based autoscaling, multi-metric evaluation, stabilization windows
- Scale API — instance readiness signaling, service health aggregation, graceful drain, instance registry
- Warm pool auto-scaling with Gateway pressure signals
- TEE hardening — KBS integration, periodic re-attestation, version-based rollback protection
- VM snapshot/restore (`snapshot create/restore/ls/rm/inspect`)
- Network isolation policies (none/strict/custom)
- Audit logging with JSON-lines trail and CLI query
- Multi-platform builds (`--platform linux/amd64,linux/arm64`)
- Compose orchestration (`compose up/down/ps/config`)
- Image signing verification (cosign-compatible)
- Seccomp profiles, no-new-privileges, capability dropping
- Prometheus metrics (18 metrics) and OpenTelemetry tracing spans

### Changed
- SDKs rewritten as native bindings (PyO3 + napi-rs)
- Vendored a3s-transport replaced with a3s-common dependency
- Large files split into focused submodules

### Fixed
- Network env vars moved from shim to entrypoint
- npm package size reduced
- macOS stub libkrun path for CI

## [0.2.0] — 2025-02-16

### Added
- Docker-compatible CLI (50 commands)
- OCI image management (pull, push, build, tag, inspect, prune)
- Dockerfile build with multi-stage support
- CRI runtime (RuntimeService + ImageService)
- Networking (bridge driver, IPAM, DNS discovery)
- Volumes (named, anonymous, tmpfs)
- Resource limits (CPU, memory, PID, ulimits via cgroup v2)
- Security options (capabilities, privileged mode, device mapping, GPU)
- Health checks, restart policies, logging drivers
- PTY support, exec, attach, top
- commit, diff, events, cp, export, save, load
- TEE core — SEV-SNP detection, configuration, shim integration
- Remote attestation — SNP report, ECDSA-P384, certificate chain, RA-TLS, simulation mode
- Sealed storage — HKDF-SHA256, AES-256-GCM, three sealing policies
- Secret injection via RA-TLS
- Rootfs caching, warm pool with TTL
- Guest init (PID 1) with exec/PTY/attestation servers

## [0.1.0] — 2025-02-15

### Added
- MicroVM runtime via libkrun (Apple HVF / Linux KVM)
- ~200ms cold start
- OCI image parser and rootfs composition
- Guest init with namespace isolation
- Vsock communication (exec, PTY, attestation)
- Cross-platform: macOS Apple Silicon, Linux x86_64/ARM64
