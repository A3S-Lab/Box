package box

import (
	"context"
	"net"
	"strings"
	"time"
)

type SandboxBuilder struct {
	client               *Client
	image                string
	timeout              time.Duration
	env                  map[string]string
	labels               map[string]string
	name                 string
	cpus                 *uint32
	memoryMiB            *uint32
	isolation            Isolation
	filesystemSnapshotID string
	workspace            string
	workdir              string
	user                 string
	hostname             string
	mounts               []Mount
	tmpfs                []TmpfsMount
	network              SandboxNetwork
	ports                []PortMapping
	dns                  []string
	hostAliases          map[string]string
	readOnly             bool
	persistent           bool
	autoRemove           bool
}

func (client *Client) Sandbox(image string) *SandboxBuilder {
	if strings.TrimSpace(image) == "" {
		image = DefaultImage
	}
	return &SandboxBuilder{
		client:      client,
		image:       image,
		timeout:     time.Hour,
		env:         make(map[string]string),
		labels:      make(map[string]string),
		isolation:   IsolationMicroVM,
		network:     TSINetwork(),
		hostAliases: make(map[string]string),
		autoRemove:  true,
	}
}

func (builder *SandboxBuilder) Timeout(timeout time.Duration) *SandboxBuilder {
	builder.timeout = timeout
	return builder
}

func (builder *SandboxBuilder) Env(key, value string) *SandboxBuilder {
	builder.env[key] = value
	return builder
}

func (builder *SandboxBuilder) Label(key, value string) *SandboxBuilder {
	builder.labels[key] = value
	return builder
}

func (builder *SandboxBuilder) Name(name string) *SandboxBuilder {
	builder.name = name
	return builder
}

func (builder *SandboxBuilder) CPUs(cpus uint32) *SandboxBuilder {
	builder.cpus = &cpus
	return builder
}

func (builder *SandboxBuilder) MemoryMiB(memory uint32) *SandboxBuilder {
	builder.memoryMiB = &memory
	return builder
}

func (builder *SandboxBuilder) Isolation(isolation Isolation) *SandboxBuilder {
	builder.isolation = isolation
	return builder
}

func (builder *SandboxBuilder) FilesystemSnapshot(snapshotID string) *SandboxBuilder {
	builder.filesystemSnapshotID = snapshotID
	return builder
}

func (builder *SandboxBuilder) Workspace(path string) *SandboxBuilder {
	builder.workspace = path
	return builder
}

func (builder *SandboxBuilder) Workdir(path string) *SandboxBuilder {
	builder.workdir = path
	return builder
}

func (builder *SandboxBuilder) User(user string) *SandboxBuilder {
	builder.user = user
	return builder
}

func (builder *SandboxBuilder) Hostname(hostname string) *SandboxBuilder {
	builder.hostname = hostname
	return builder
}

func (builder *SandboxBuilder) Mount(mount Mount) *SandboxBuilder {
	builder.mounts = append(builder.mounts, mount)
	return builder
}

func (builder *SandboxBuilder) Tmpfs(mount TmpfsMount) *SandboxBuilder {
	builder.tmpfs = append(builder.tmpfs, mount)
	return builder
}

func (builder *SandboxBuilder) Network(network SandboxNetwork) *SandboxBuilder {
	builder.network = network
	return builder
}

func (builder *SandboxBuilder) PublishTCP(hostPort, guestPort uint16) *SandboxBuilder {
	builder.ports = append(builder.ports, TCPPort(hostPort, guestPort))
	return builder
}

func (builder *SandboxBuilder) DNSServer(address string) *SandboxBuilder {
	builder.dns = append(builder.dns, address)
	return builder
}

func (builder *SandboxBuilder) HostAlias(host, address string) *SandboxBuilder {
	builder.hostAliases[host] = address
	return builder
}

func (builder *SandboxBuilder) ReadOnly(enabled bool) *SandboxBuilder {
	builder.readOnly = enabled
	return builder
}

func (builder *SandboxBuilder) Persistent(enabled bool) *SandboxBuilder {
	builder.persistent = enabled
	return builder
}

func (builder *SandboxBuilder) AutoRemove(enabled bool) *SandboxBuilder {
	builder.autoRemove = enabled
	return builder
}

func (builder *SandboxBuilder) Start(ctx context.Context) (*Sandbox, error) {
	const op = "sandbox_create"
	if builder == nil || builder.client == nil {
		return nil, invalid(op, "sandbox builder is not initialized")
	}
	if err := builder.validate(); err != nil {
		return nil, err
	}
	mounts := make([]map[string]any, 0, len(builder.mounts))
	for _, mount := range builder.mounts {
		mounts = append(mounts, mount.bridgeValue())
	}
	tmpfs := make([]map[string]any, 0, len(builder.tmpfs))
	for _, mount := range builder.tmpfs {
		tmpfs = append(tmpfs, mount.bridgeValue())
	}
	fields := map[string]any{
		"image":           builder.image,
		"timeout_seconds": durationSeconds(builder.timeout),
		"env":             cloneStringMap(builder.env),
		"labels":          cloneStringMap(builder.labels),
		"isolation":       builder.isolation,
		"mounts":          mounts,
		"tmpfs":           tmpfs,
		"network":         builder.network.bridgeValue(),
		"ports":           append([]PortMapping{}, builder.ports...),
		"dns":             append([]string{}, builder.dns...),
		"host_aliases":    cloneStringMap(builder.hostAliases),
		"read_only":       builder.readOnly,
		"persistent":      builder.persistent,
		"auto_remove":     builder.autoRemove,
	}
	if builder.name != "" {
		fields["name"] = builder.name
	}
	if builder.cpus != nil {
		fields["cpus"] = *builder.cpus
	}
	if builder.memoryMiB != nil {
		fields["memory_mb"] = *builder.memoryMiB
	}
	if builder.filesystemSnapshotID != "" {
		fields["filesystem_snapshot_id"] = builder.filesystemSnapshotID
	}
	if builder.workspace != "" {
		fields["workspace"] = builder.workspace
	}
	if builder.workdir != "" {
		fields["workdir"] = builder.workdir
	}
	if builder.user != "" {
		fields["user"] = builder.user
	}
	if builder.hostname != "" {
		fields["hostname"] = builder.hostname
	}
	var info SandboxInfo
	if err := builder.client.request(ctx, op, fields, &info); err != nil {
		return nil, err
	}
	if err := validateSandboxInfo(op, "", builder.isolation, info); err != nil {
		return nil, err
	}
	return newSandbox(builder.client.runtime, info), nil
}

func (builder *SandboxBuilder) validate() error {
	const op = "sandbox_create"
	if strings.TrimSpace(builder.image) == "" {
		return invalid(op, "sandbox image cannot be empty")
	}
	if builder.timeout <= 0 {
		return invalid(op, "sandbox timeout must be greater than zero")
	}
	if builder.cpus != nil && *builder.cpus == 0 {
		return invalid(op, "CPU count must be greater than zero")
	}
	if builder.memoryMiB != nil && *builder.memoryMiB == 0 {
		return invalid(op, "memory must be greater than zero")
	}
	if builder.isolation != IsolationMicroVM && builder.isolation != IsolationSandbox {
		return invalid(op, "isolation must be microvm or sandbox")
	}
	optionalValues := map[string]string{
		"sandbox name":           builder.name,
		"filesystem snapshot ID": builder.filesystemSnapshotID,
		"workspace":              builder.workspace,
		"working directory":      builder.workdir,
		"user":                   builder.user,
		"hostname":               builder.hostname,
	}
	for name, value := range optionalValues {
		if value != "" && strings.TrimSpace(value) == "" {
			return invalid(op, name+" cannot be blank")
		}
	}
	if err := validateLabels(op, builder.labels); err != nil {
		return err
	}
	for key := range builder.env {
		if strings.TrimSpace(key) == "" {
			return invalid(op, "environment variable name cannot be empty")
		}
	}
	if err := builder.network.validate(); err != nil {
		return err
	}
	targets := make(map[string]struct{}, len(builder.mounts)+len(builder.tmpfs))
	for _, mount := range builder.mounts {
		if err := mount.validate(); err != nil {
			return err
		}
		if _, duplicate := targets[mount.target]; duplicate {
			return invalid(op, "mount targets must be unique")
		}
		targets[mount.target] = struct{}{}
	}
	for _, mount := range builder.tmpfs {
		if err := mount.validate(); err != nil {
			return err
		}
		if _, duplicate := targets[mount.target]; duplicate {
			return invalid(op, "mount targets must be unique")
		}
		targets[mount.target] = struct{}{}
	}
	for _, port := range builder.ports {
		if port.GuestPort == 0 {
			return invalid(op, "published guest TCP ports must be greater than zero")
		}
	}
	for _, address := range builder.dns {
		if net.ParseIP(address) == nil {
			return invalid(op, "DNS servers must be IP addresses")
		}
	}
	for host, address := range builder.hostAliases {
		if strings.TrimSpace(host) == "" || net.ParseIP(address) == nil {
			return invalid(op, "host aliases require a host name and IP address")
		}
	}
	return nil
}

func durationSeconds(duration time.Duration) uint64 {
	seconds := duration / time.Second
	if duration%time.Second != 0 {
		seconds++
	}
	return uint64(seconds)
}
