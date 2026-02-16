# Box Next Steps

## Completed ✅

- [x] Add a3s-transport dependency to Box workspace
- [x] Migrate Exec server (guest + host) to Frame protocol
- [x] Migrate PTY protocol (host-side FrameReader/FrameWriter)
- [x] Migrate Attest protocol (Frame inside TLS tunnel)
- [x] Embedded sandbox SDK (`a3s-box-sdk` crate: BoxSdk, Sandbox, SandboxOptions)

## Next

- [ ] Guest-side TEE self-detection API
- [ ] AgentClient health check migration (HTTP → Frame)
