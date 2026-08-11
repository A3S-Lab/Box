package box

// Capabilities describes the local machine bridge paired with this SDK.
type Capabilities struct {
	ProtocolVersion int      `json:"protocol_version"`
	Operations      []string `json:"operations"`
}

type BuildImageInfo struct {
	Reference  string `json:"reference"`
	Digest     string `json:"digest"`
	SizeBytes  uint64 `json:"size_bytes"`
	LayerCount int    `json:"layer_count"`
}

type ImageInfo struct {
	Reference string `json:"reference"`
	Digest    string `json:"digest"`
	SizeBytes uint64 `json:"size_bytes"`
	PulledAt  string `json:"pulled_at"`
	LastUsed  string `json:"last_used"`
	Path      string `json:"path"`
}

type ImageHealthCheckInfo struct {
	Test        []string `json:"test"`
	Interval    *uint64  `json:"interval"`
	Timeout     *uint64  `json:"timeout"`
	Retries     *uint64  `json:"retries"`
	StartPeriod *uint64  `json:"start_period"`
}

type ImageInspectInfo struct {
	ImageInfo
	ManifestDigest string                `json:"manifest_digest"`
	LayerCount     int                   `json:"layer_count"`
	Entrypoint     []string              `json:"entrypoint"`
	Command        []string              `json:"command"`
	Env            map[string]string     `json:"env"`
	WorkingDir     *string               `json:"working_dir"`
	User           *string               `json:"user"`
	ExposedPorts   []string              `json:"exposed_ports"`
	Volumes        []string              `json:"volumes"`
	StopSignal     *string               `json:"stop_signal"`
	HealthCheck    *ImageHealthCheckInfo `json:"health_check"`
	Onbuild        []string              `json:"onbuild"`
	Labels         map[string]string     `json:"labels"`
}

type ImageHistoryInfo struct {
	Created    *string `json:"created"`
	CreatedBy  string  `json:"created_by"`
	SizeBytes  uint64  `json:"size_bytes"`
	Comment    string  `json:"comment"`
	EmptyLayer bool    `json:"empty_layer"`
}

type PushImageInfo struct {
	Reference      string `json:"reference"`
	ManifestDigest string `json:"manifest_digest"`
	ConfigURL      string `json:"config_url"`
	ManifestURL    string `json:"manifest_url"`
}

type VolumeInfo struct {
	Name       string            `json:"name"`
	Driver     string            `json:"driver"`
	MountPoint string            `json:"mount_point"`
	Labels     map[string]string `json:"labels"`
	InUseBy    []string          `json:"in_use_by"`
	InUse      bool              `json:"in_use"`
	SizeLimit  uint64            `json:"size_limit"`
	CreatedAt  string            `json:"created_at"`
}

type NetworkEndpointInfo struct {
	BoxID      string   `json:"box_id"`
	BoxName    string   `json:"box_name"`
	Aliases    []string `json:"aliases"`
	IPAddress  string   `json:"ip_address"`
	MACAddress string   `json:"mac_address"`
}

type NetworkInfo struct {
	Name          string                `json:"name"`
	Driver        string                `json:"driver"`
	Subnet        string                `json:"subnet"`
	Gateway       string                `json:"gateway"`
	Labels        map[string]string     `json:"labels"`
	Endpoints     []NetworkEndpointInfo `json:"endpoints"`
	EndpointCount int                   `json:"endpoint_count"`
	Isolation     string                `json:"isolation"`
	CreatedAt     string                `json:"created_at"`
}

type SandboxSummary struct {
	ID            string            `json:"id"`
	ShortID       string            `json:"short_id"`
	Name          string            `json:"name"`
	Image         string            `json:"image"`
	Isolation     string            `json:"isolation"`
	Status        string            `json:"status"`
	StatusSummary string            `json:"status_summary"`
	Active        bool              `json:"active"`
	PID           *int              `json:"pid"`
	CPUs          uint32            `json:"cpus"`
	MemoryMiB     uint32            `json:"memory_mb"`
	Ports         []string          `json:"ports"`
	Command       []string          `json:"command"`
	Health        string            `json:"health"`
	Labels        map[string]string `json:"labels"`
	CreatedAt     string            `json:"created_at"`
	StartedAt     *string           `json:"started_at"`
	NetworkName   *string           `json:"network_name"`
	VolumeNames   []string          `json:"volume_names"`
}

type SandboxLogEntry struct {
	Stream    string  `json:"stream"`
	Message   string  `json:"log"`
	Timestamp *string `json:"time"`
}

type SandboxStats struct {
	ID               string  `json:"id"`
	ShortID          string  `json:"short_id"`
	Name             string  `json:"name"`
	Status           string  `json:"status"`
	PID              int     `json:"pid"`
	CPUs             uint32  `json:"cpus"`
	CPUPercent       float64 `json:"cpu_percent"`
	CPUPercentScaled float64 `json:"cpu_percent_scaled"`
	MemoryBytes      uint64  `json:"memory_bytes"`
	MemoryLimitBytes uint64  `json:"memory_limit_bytes"`
	MemoryPercent    float64 `json:"memory_percent"`
	NetworkRXBytes   uint64  `json:"network_rx_bytes"`
	NetworkTXBytes   uint64  `json:"network_tx_bytes"`
	BlockReadBytes   uint64  `json:"block_read_bytes"`
	BlockWriteBytes  uint64  `json:"block_write_bytes"`
}

type ExecutionProcessInfo struct {
	ProcessID string  `json:"process_id"`
	PID       *uint32 `json:"pid"`
	Terminal  bool    `json:"terminal"`
}

type ExecutionProcessInventory struct {
	ExecutionID string                 `json:"execution_id"`
	Generation  uint64                 `json:"generation"`
	Processes   []ExecutionProcessInfo `json:"processes"`
}

type ExecutionCPUStats struct {
	UsageNS     uint64 `json:"usage_ns"`
	UserNS      uint64 `json:"user_ns"`
	SystemNS    uint64 `json:"system_ns"`
	ThrottledNS uint64 `json:"throttled_ns"`
}

type ExecutionMemoryStats struct {
	UsageBytes uint64  `json:"usage_bytes"`
	LimitBytes *uint64 `json:"limit_bytes"`
	PeakBytes  *uint64 `json:"peak_bytes"`
}

type ExecutionStats struct {
	ExecutionID     string               `json:"execution_id"`
	Generation      uint64               `json:"generation"`
	TimestampUnixNS uint64               `json:"timestamp_unix_ns"`
	CPU             ExecutionCPUStats    `json:"cpu"`
	Memory          ExecutionMemoryStats `json:"memory"`
	ProcessCount    uint64               `json:"process_count"`
	Metrics         map[string]uint64    `json:"metrics"`
}

type ExecutionEventKind string

const (
	EventContainerCreating ExecutionEventKind = "container-creating"
	EventContainerCreated  ExecutionEventKind = "container-created"
	EventContainerStarted  ExecutionEventKind = "container-started"
	EventContainerStopped  ExecutionEventKind = "container-stopped"
	EventContainerDeleted  ExecutionEventKind = "container-deleted"
	EventContainerPaused   ExecutionEventKind = "container-paused"
	EventContainerResumed  ExecutionEventKind = "container-resumed"
	EventResourcesUpdated  ExecutionEventKind = "resources-updated"
	EventProcessCreated    ExecutionEventKind = "process-created"
	EventProcessStarted    ExecutionEventKind = "process-started"
	EventProcessExited     ExecutionEventKind = "process-exited"
	EventOutputDropped     ExecutionEventKind = "output-dropped"
	EventRuntimeWarning    ExecutionEventKind = "runtime-warning"
)

type ExecutionRuntimeEvent struct {
	Sequence        uint64             `json:"sequence"`
	TimestampUnixNS uint64             `json:"timestamp_unix_ns"`
	ProcessID       *string            `json:"process_id"`
	Kind            ExecutionEventKind `json:"kind"`
	Attributes      map[string]string  `json:"attributes"`
}

type ExecutionEventBatch struct {
	ExecutionID  string                  `json:"execution_id"`
	Generation   uint64                  `json:"generation"`
	Events       []ExecutionRuntimeEvent `json:"events"`
	NextSequence uint64                  `json:"next_sequence"`
}

type ExecutionEventsRequest struct {
	AfterSequence uint64
	Limit         uint32
	WaitTimeoutMS *uint64
}

type ExecutionResourceUpdate struct {
	MemoryReservation *uint64 `json:"memory_reservation,omitempty"`
	MemorySwap        *int64  `json:"memory_swap,omitempty"`
	PIDsLimit         *uint64 `json:"pids_limit,omitempty"`
	CPUShares         *uint64 `json:"cpu_shares,omitempty"`
	CPUQuota          *int64  `json:"cpu_quota,omitempty"`
	CPUPeriod         *uint64 `json:"cpu_period,omitempty"`
	CPUSetCPUs        *string `json:"cpuset_cpus,omitempty"`
}

type RuntimeVirtualization struct {
	Available bool    `json:"available"`
	Backend   *string `json:"backend"`
	Details   string  `json:"details"`
}

type RuntimeDiagnostics struct {
	CoreVersion    string                `json:"core_version"`
	RuntimeVersion string                `json:"runtime_version"`
	SDKVersion     string                `json:"sdk_version"`
	Home           string                `json:"home"`
	Virtualization RuntimeVirtualization `json:"virtualization"`
}

type RuntimeDiskUsage struct {
	Home           string `json:"home"`
	TotalBytes     uint64 `json:"total_bytes"`
	BoxesBytes     uint64 `json:"boxes_bytes"`
	ImagesBytes    uint64 `json:"images_bytes"`
	VolumesBytes   uint64 `json:"volumes_bytes"`
	SnapshotsBytes uint64 `json:"snapshots_bytes"`
	StateBytes     uint64 `json:"state_bytes"`
	OtherBytes     uint64 `json:"other_bytes"`
}

type FilesystemSnapshotSummary struct {
	ID              string            `json:"id"`
	Name            string            `json:"name"`
	SourceSandboxID string            `json:"source_box_id"`
	Image           string            `json:"image"`
	VCPUs           uint32            `json:"vcpus"`
	MemoryMiB       uint32            `json:"memory_mb"`
	Volumes         []string          `json:"volumes"`
	Command         []string          `json:"command"`
	Ports           []string          `json:"port_map"`
	Labels          map[string]string `json:"labels"`
	NetworkMode     *string           `json:"network_mode"`
	SizeBytes       uint64            `json:"size_bytes"`
	CreatedAt       string            `json:"created_at"`
	Description     string            `json:"description"`
}

type FilesystemSnapshotInfo struct {
	SnapshotID string       `json:"snapshot_id"`
	SizeBytes  uint64       `json:"size_bytes"`
	State      SandboxState `json:"state"`
	Generation uint64       `json:"generation"`
}

type SandboxInfo struct {
	SandboxID  string       `json:"sandbox_id"`
	Generation uint64       `json:"generation"`
	State      SandboxState `json:"state"`
	Isolation  Isolation    `json:"isolation"`
}

type WriteInfo struct {
	Path string `json:"path"`
	Size uint64 `json:"size"`
}

// Artifact is one verified, bounded guest file and its SHA-256 digest.
type Artifact struct {
	Path     string
	Data     []byte
	Size     uint64
	SHA256   string
	HostPath string
}

type EntryInfo struct {
	Name            string  `json:"name"`
	Type            string  `json:"type"`
	Path            string  `json:"path"`
	Size            uint64  `json:"size"`
	Mode            uint32  `json:"mode"`
	Permissions     string  `json:"permissions"`
	Owner           string  `json:"owner"`
	Group           string  `json:"group"`
	ModifiedSeconds int64   `json:"modified_seconds"`
	ModifiedNanos   int32   `json:"modified_nanos"`
	SymlinkTarget   *string `json:"symlink_target"`
}

type CommandResult struct {
	Stdout    []byte
	Stderr    []byte
	ExitCode  int
	Truncated bool
}

func (result CommandResult) StdoutString() string { return string(result.Stdout) }
func (result CommandResult) StderrString() string { return string(result.Stderr) }
