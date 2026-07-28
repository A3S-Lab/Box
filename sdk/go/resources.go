package box

import (
	"context"
	"strings"
)

func (client *Client) GetVolume(ctx context.Context, name string) (*VolumeInfo, error) {
	const op = "volume_get"
	if strings.TrimSpace(name) == "" {
		return nil, invalid(op, "volume name cannot be empty")
	}
	var result struct {
		Volume *VolumeInfo `json:"volume"`
	}
	if err := client.request(ctx, op, map[string]any{"name": name}, &result); err != nil {
		return nil, err
	}
	return result.Volume, nil
}

func (client *Client) ListVolumes(ctx context.Context) ([]VolumeInfo, error) {
	var result struct {
		Volumes []VolumeInfo `json:"volumes"`
	}
	if err := client.request(ctx, "volume_list", nil, &result); err != nil {
		return nil, err
	}
	return result.Volumes, nil
}

func (client *Client) RemoveVolume(ctx context.Context, name string, force bool) (VolumeInfo, error) {
	const op = "volume_remove"
	if strings.TrimSpace(name) == "" {
		return VolumeInfo{}, invalid(op, "volume name cannot be empty")
	}
	var volume VolumeInfo
	err := client.request(ctx, op, map[string]any{"name": name, "force": force}, &volume)
	return volume, err
}

func (client *Client) PruneVolumes(ctx context.Context) ([]string, error) {
	var result struct {
		Names []string `json:"names"`
	}
	if err := client.request(ctx, "volume_prune", nil, &result); err != nil {
		return nil, err
	}
	return result.Names, nil
}

func (client *Client) GetNetwork(ctx context.Context, name string) (*NetworkInfo, error) {
	const op = "network_get"
	if strings.TrimSpace(name) == "" {
		return nil, invalid(op, "network name cannot be empty")
	}
	var result struct {
		Network *NetworkInfo `json:"network"`
	}
	if err := client.request(ctx, op, map[string]any{"name": name}, &result); err != nil {
		return nil, err
	}
	return result.Network, nil
}

func (client *Client) ListNetworks(ctx context.Context) ([]NetworkInfo, error) {
	var result struct {
		Networks []NetworkInfo `json:"networks"`
	}
	if err := client.request(ctx, "network_list", nil, &result); err != nil {
		return nil, err
	}
	return result.Networks, nil
}

func (client *Client) RemoveNetwork(ctx context.Context, name string) (NetworkInfo, error) {
	const op = "network_remove"
	if strings.TrimSpace(name) == "" {
		return NetworkInfo{}, invalid(op, "network name cannot be empty")
	}
	var network NetworkInfo
	err := client.request(ctx, op, map[string]any{"name": name}, &network)
	return network, err
}

func (client *Client) PruneNetworks(ctx context.Context) ([]string, error) {
	var result struct {
		Names []string `json:"names"`
	}
	if err := client.request(ctx, "network_prune", nil, &result); err != nil {
		return nil, err
	}
	return result.Names, nil
}

func (client *Client) ListSandboxes(ctx context.Context, all bool) ([]SandboxSummary, error) {
	var result struct {
		Sandboxes []SandboxSummary `json:"sandboxes"`
	}
	if err := client.request(ctx, "sandbox_list", map[string]any{"all": all}, &result); err != nil {
		return nil, err
	}
	return result.Sandboxes, nil
}

func (client *Client) GetSandbox(ctx context.Context, query string) (*SandboxSummary, error) {
	const op = "sandbox_get"
	if strings.TrimSpace(query) == "" {
		return nil, invalid(op, "sandbox query cannot be empty")
	}
	var result struct {
		Sandbox *SandboxSummary `json:"sandbox"`
	}
	if err := client.request(ctx, op, map[string]any{"query": query}, &result); err != nil {
		return nil, err
	}
	return result.Sandbox, nil
}

func (client *Client) RuntimeDiagnostics(ctx context.Context) (RuntimeDiagnostics, error) {
	var result RuntimeDiagnostics
	err := client.request(ctx, "runtime_diagnostics", nil, &result)
	return result, err
}

func (client *Client) RuntimeDiskUsage(ctx context.Context) (RuntimeDiskUsage, error) {
	var result RuntimeDiskUsage
	err := client.request(ctx, "runtime_disk_usage", nil, &result)
	return result, err
}

func (client *Client) ListFilesystemSnapshots(ctx context.Context) ([]FilesystemSnapshotSummary, error) {
	var result struct {
		Snapshots []FilesystemSnapshotSummary `json:"snapshots"`
	}
	if err := client.request(ctx, "filesystem_snapshot_list", nil, &result); err != nil {
		return nil, err
	}
	return result.Snapshots, nil
}

func (client *Client) GetFilesystemSnapshot(
	ctx context.Context,
	snapshotID string,
) (*FilesystemSnapshotSummary, error) {
	const op = "filesystem_snapshot_get"
	if strings.TrimSpace(snapshotID) == "" {
		return nil, invalid(op, "snapshot ID cannot be empty")
	}
	var result struct {
		Snapshot *FilesystemSnapshotSummary `json:"snapshot"`
	}
	if err := client.request(ctx, op, map[string]any{"snapshot_id": snapshotID}, &result); err != nil {
		return nil, err
	}
	return result.Snapshot, nil
}

// FilesystemSnapshotSize returns (size, true, nil) when a snapshot exists.
func (client *Client) FilesystemSnapshotSize(ctx context.Context, snapshotID string) (uint64, bool, error) {
	const op = "filesystem_snapshot_size"
	if strings.TrimSpace(snapshotID) == "" {
		return 0, false, invalid(op, "snapshot ID cannot be empty")
	}
	var result struct {
		SizeBytes *uint64 `json:"size_bytes"`
	}
	if err := client.request(ctx, op, map[string]any{"snapshot_id": snapshotID}, &result); err != nil {
		return 0, false, err
	}
	if result.SizeBytes == nil {
		return 0, false, nil
	}
	return *result.SizeBytes, true, nil
}

func (client *Client) DeleteFilesystemSnapshot(ctx context.Context, snapshotID string) (bool, error) {
	const op = "filesystem_snapshot_delete"
	if strings.TrimSpace(snapshotID) == "" {
		return false, invalid(op, "snapshot ID cannot be empty")
	}
	var result struct {
		Deleted bool `json:"deleted"`
	}
	if err := client.request(ctx, op, map[string]any{"snapshot_id": snapshotID}, &result); err != nil {
		return false, err
	}
	return result.Deleted, nil
}
