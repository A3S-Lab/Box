// Package bridge contains the versioned machine bridge contract shared by the
// public Go API and the installed A3S Box binary.
package bridge

import "encoding/json"

const ProtocolVersion = 4

// RequiredOperations is the complete operation set used by this SDK version.
// New clients verify this inventory before issuing a mutating request.
var RequiredOperations = []string{
	"sdk_capabilities",
	"runtime_diagnostics",
	"runtime_disk_usage",
	"image_build",
	"image_pull",
	"image_get",
	"image_list",
	"image_inspect",
	"image_history",
	"image_tag",
	"image_push",
	"image_remove",
	"image_evict",
	"volume_create",
	"volume_get",
	"volume_list",
	"volume_remove",
	"volume_prune",
	"network_create",
	"network_get",
	"network_list",
	"network_remove",
	"network_prune",
	"sandbox_list",
	"sandbox_get",
	"sandbox_create",
	"sandbox_inspect",
	"sandbox_stop",
	"sandbox_restart",
	"sandbox_remove",
	"sandbox_kill",
	"sandbox_pause",
	"sandbox_resume",
	"sandbox_logs",
	"sandbox_stats",
	"sandbox_processes",
	"sandbox_runtime_stats",
	"sandbox_events",
	"sandbox_update_resources",
	"sandbox_snapshot_create",
	"filesystem_snapshot_list",
	"filesystem_snapshot_get",
	"filesystem_snapshot_size",
	"filesystem_snapshot_delete",
	"command_run",
	"file_write",
	"file_read",
	"filesystem_stat",
	"filesystem_list",
	"filesystem_make_dir",
	"filesystem_move",
	"filesystem_remove",
}

type Envelope struct {
	ProtocolVersion int             `json:"protocol_version"`
	OK              bool            `json:"ok"`
	Result          json.RawMessage `json:"result"`
	Error           *RemoteError    `json:"error"`
}

type RemoteError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}
