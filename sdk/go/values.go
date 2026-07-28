package box

import "strings"

const DefaultImage = "alpine:3.20"

type Isolation string

const (
	IsolationMicroVM Isolation = "microvm"
	IsolationSandbox Isolation = "sandbox"
)

type SandboxState string

const (
	StateCreated  SandboxState = "created"
	StateCreating SandboxState = "creating"
	StateRunning  SandboxState = "running"
	StatePaused   SandboxState = "paused"
	StateStopped  SandboxState = "stopped"
	StateFailed   SandboxState = "failed"
	StateKilled   SandboxState = "killed"
	StateRemoved  SandboxState = "removed"
)

// RegistryCredentials keeps the password out of default formatting while
// still allowing it to be passed through bridge stdin.
type RegistryCredentials struct {
	username string
	password string
}

func BasicCredentials(username, password string) RegistryCredentials {
	return RegistryCredentials{username: username, password: password}
}

func (credentials RegistryCredentials) Username() string { return credentials.username }

func (credentials RegistryCredentials) String() string {
	return "RegistryCredentials{Username:" + credentials.username + " Password:<redacted>}"
}

func (credentials RegistryCredentials) GoString() string { return credentials.String() }

func (credentials RegistryCredentials) bridgeValue() map[string]string {
	return map[string]string{
		"username": credentials.username,
		"password": credentials.password,
	}
}

func (credentials RegistryCredentials) validate() error {
	if strings.TrimSpace(credentials.username) == "" {
		return invalid("registry_credentials", "registry username cannot be empty")
	}
	return nil
}

type SignaturePolicy struct {
	mode      string
	publicKey string
	issuer    string
	identity  string
}

func SkipSignatures() SignaturePolicy { return SignaturePolicy{mode: "skip"} }

func CosignKey(publicKey string) SignaturePolicy {
	return SignaturePolicy{mode: "cosign_key", publicKey: publicKey}
}

func CosignKeyless(issuer, identity string) SignaturePolicy {
	return SignaturePolicy{mode: "cosign_keyless", issuer: issuer, identity: identity}
}

func (policy SignaturePolicy) bridgeValue() map[string]string {
	value := map[string]string{"mode": policy.mode}
	if policy.publicKey != "" {
		value["public_key"] = policy.publicKey
	}
	if policy.issuer != "" {
		value["issuer"] = policy.issuer
	}
	if policy.identity != "" {
		value["identity"] = policy.identity
	}
	return value
}

func (policy SignaturePolicy) validate() error {
	switch policy.mode {
	case "skip":
		return nil
	case "cosign_key":
		if strings.TrimSpace(policy.publicKey) == "" {
			return invalid("signature_policy", "cosign public key cannot be empty")
		}
		return nil
	case "cosign_keyless":
		if strings.TrimSpace(policy.issuer) == "" || strings.TrimSpace(policy.identity) == "" {
			return invalid("signature_policy", "keyless issuer and identity cannot be empty")
		}
		return nil
	default:
		return invalid("signature_policy", "signature policy is not configured")
	}
}

type RegistryProtocol string

const (
	RegistryHTTPS RegistryProtocol = "https"
	RegistryHTTP  RegistryProtocol = "http"
)

type Mount struct {
	kind     string
	source   string
	target   string
	readOnly bool
}

func BindMount(source, target string) Mount {
	return Mount{kind: "bind", source: source, target: target}
}

func NamedVolume(name, target string) Mount {
	return Mount{kind: "named", source: name, target: target}
}

func (mount Mount) ReadOnly() Mount {
	mount.readOnly = true
	return mount
}

func (mount Mount) bridgeValue() map[string]any {
	value := map[string]any{
		"kind":      mount.kind,
		"target":    mount.target,
		"read_only": mount.readOnly,
	}
	if mount.kind == "bind" {
		value["source"] = mount.source
	} else {
		value["name"] = mount.source
	}
	return value
}

func (mount Mount) validate() error {
	if mount.kind != "bind" && mount.kind != "named" {
		return invalid("sandbox_create", "mount kind must be bind or named")
	}
	if strings.TrimSpace(mount.source) == "" || strings.TrimSpace(mount.target) == "" {
		return invalid("sandbox_create", "mount source and target cannot be empty")
	}
	return nil
}

type TmpfsMount struct {
	target    string
	sizeBytes *uint64
	readOnly  bool
}

func Tmpfs(target string) TmpfsMount { return TmpfsMount{target: target} }

func (mount TmpfsMount) SizeBytes(size uint64) TmpfsMount {
	mount.sizeBytes = &size
	return mount
}

func (mount TmpfsMount) ReadOnly() TmpfsMount {
	mount.readOnly = true
	return mount
}

func (mount TmpfsMount) bridgeValue() map[string]any {
	value := map[string]any{"target": mount.target, "read_only": mount.readOnly}
	if mount.sizeBytes != nil {
		value["size_bytes"] = *mount.sizeBytes
	}
	return value
}

func (mount TmpfsMount) validate() error {
	if strings.TrimSpace(mount.target) == "" {
		return invalid("sandbox_create", "tmpfs target cannot be empty")
	}
	if mount.sizeBytes != nil && *mount.sizeBytes == 0 {
		return invalid("sandbox_create", "tmpfs size must be greater than zero")
	}
	return nil
}

type SandboxNetwork struct {
	mode string
	name string
}

func TSINetwork() SandboxNetwork { return SandboxNetwork{mode: "tsi"} }
func NoNetwork() SandboxNetwork  { return SandboxNetwork{mode: "none"} }

func BridgeNetwork(name string) SandboxNetwork {
	return SandboxNetwork{mode: "bridge", name: name}
}

func (network SandboxNetwork) bridgeValue() map[string]string {
	value := map[string]string{"mode": network.mode}
	if network.name != "" {
		value["name"] = network.name
	}
	return value
}

func (network SandboxNetwork) validate() error {
	switch network.mode {
	case "tsi", "none":
		return nil
	case "bridge":
		if strings.TrimSpace(network.name) == "" {
			return invalid("sandbox_create", "bridge network name cannot be empty")
		}
		return nil
	default:
		return invalid("sandbox_create", "sandbox network is not configured")
	}
}

type PortMapping struct {
	HostPort  uint16 `json:"host_port"`
	GuestPort uint16 `json:"guest_port"`
}

func TCPPort(hostPort, guestPort uint16) PortMapping {
	return PortMapping{HostPort: hostPort, GuestPort: guestPort}
}
