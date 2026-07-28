// Command local-sdk-smoke exercises the public Go SDK against one real local
// A3S Box isolation backend. It is invoked by scripts/local-sdk-smoke.sh.
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	box "github.com/A3S-Lab/Box/sdk/go/v3"
)

func main() {
	isolation, err := selectedIsolation()
	if err == nil {
		ctx, cancel := context.WithTimeout(context.Background(), 15*time.Minute)
		defer cancel()
		err = run(ctx, isolation)
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "Go SDK smoke failed: %v\n", err)
		os.Exit(1)
	}
}

func run(ctx context.Context, isolation box.Isolation) (returnErr error) {
	client, err := box.NewClient(ctx)
	if err != nil {
		return err
	}
	diagnostics, err := client.RuntimeDiagnostics(ctx)
	if err != nil {
		return err
	}
	if diagnostics.Home != os.Getenv("A3S_HOME") || diagnostics.RuntimeVersion == "" {
		return fmt.Errorf("runtime diagnostics did not describe the smoke runtime: %+v", diagnostics)
	}
	if _, err := client.RuntimeDiskUsage(ctx); err != nil {
		return err
	}
	if !slices.Contains(client.Capabilities().Operations, "image_push") {
		return errors.New("runtime capability inventory is incomplete")
	}

	contextDir := filepath.Join(os.Getenv("A3S_HOME"), "go-sdk-build-context")
	if err := os.MkdirAll(contextDir, 0o755); err != nil {
		return err
	}
	defer func() { returnErr = errors.Join(returnErr, os.RemoveAll(contextDir)) }()
	if err := os.WriteFile(
		filepath.Join(contextDir, "Dockerfile"),
		[]byte("FROM alpine:3.20\nENV A3S_SDK_BASE=ready\nWORKDIR /workspace\n"),
		0o644,
	); err != nil {
		return err
	}

	image, err := client.Image(contextDir).Tag("local/a3s-sdk-smoke-go:latest").Build(ctx)
	if err != nil {
		return err
	}
	defer func() {
		cleanup, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		returnErr = errors.Join(returnErr, client.RemoveImage(cleanup, image.Reference))
		_, evictErr := client.EvictImages(cleanup)
		returnErr = errors.Join(returnErr, evictErr)
	}()
	if value, err := client.GetImage(ctx, image.Reference); err != nil || value == nil {
		return fmt.Errorf("built image is not gettable: %w", err)
	}
	if value, err := client.InspectImage(ctx, image.Reference); err != nil || value == nil {
		return fmt.Errorf("built image is not inspectable: %w", err)
	}
	if value, err := client.ImageHistory(ctx, image.Reference); err != nil || value == nil {
		return fmt.Errorf("built image history is unavailable: %w", err)
	}
	tagged, err := client.TagImage(ctx, image.Reference, "local/a3s-sdk-smoke-go:tested")
	if err != nil {
		return err
	}
	if err := client.RemoveImage(ctx, tagged.Reference); err != nil {
		return err
	}

	pruneVolume, err := client.Volume("go-sdk-prune-cache").Create(ctx)
	if err != nil {
		return err
	}
	prunedVolumes, err := client.PruneVolumes(ctx)
	if err != nil || !slices.Contains(prunedVolumes, pruneVolume.Name) {
		return fmt.Errorf("volume prune did not remove %q: %w", pruneVolume.Name, err)
	}
	pruneNetwork, err := client.Network("go-sdk-prune-network").Subnet("10.89.96.0/24").Create(ctx)
	if err != nil {
		return err
	}
	prunedNetworks, err := client.PruneNetworks(ctx)
	if err != nil || !slices.Contains(prunedNetworks, pruneNetwork.Name) {
		return fmt.Errorf("network prune did not remove %q: %w", pruneNetwork.Name, err)
	}

	volume, err := client.Volume("go-sdk-cache").Label("purpose", "sdk-smoke").Create(ctx)
	if err != nil {
		return err
	}
	defer func() {
		cleanup, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		_, err := client.RemoveVolume(cleanup, volume.Name, false)
		returnErr = errors.Join(returnErr, err)
	}()

	var network *box.NetworkInfo
	builder := client.Sandbox(image.Reference).
		Isolation(isolation).
		Mount(box.NamedVolume(volume.Name, "/cache")).
		Workdir("/workspace")
	if isolation == box.IsolationMicroVM {
		created, err := client.Network("go-sdk-network").Subnet("10.89.94.0/24").Create(ctx)
		if err != nil {
			return err
		}
		network = &created
		defer func() {
			cleanup, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()
			_, err := client.RemoveNetwork(cleanup, network.Name)
			returnErr = errors.Join(returnErr, err)
		}()
		builder = builder.Network(box.BridgeNetwork(network.Name)).PublishTCP(0, 8080)
	} else {
		builder = builder.Network(box.NoNetwork())
	}

	sandbox, err := builder.Start(ctx)
	if err != nil {
		return err
	}
	defer func() {
		cleanup, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		returnErr = errors.Join(returnErr, sandbox.Close(cleanup))
	}()
	if _, err := client.ConnectSandbox(ctx, sandbox.ID()); err != nil {
		return err
	}
	listed, err := client.ListSandboxes(ctx, true)
	if err != nil || !hasSandbox(listed, sandbox.ID()) {
		return fmt.Errorf("running Sandbox is absent from inventory: %w", err)
	}
	if value, err := client.GetSandbox(ctx, sandbox.ID()); err != nil || value == nil {
		return fmt.Errorf("running Sandbox is not gettable: %w", err)
	}

	command, err := sandbox.Run(ctx, box.Argv("printf", "go-sdk-ok"))
	if err != nil || command.ExitCode != 0 || command.StdoutString() != "go-sdk-ok" {
		return fmt.Errorf("foreground command failed: result=%+v error=%w", command, err)
	}
	script, err := sandbox.Script("printf 'go-script-ok'\n").Env("CI", "true").Run(ctx)
	if err != nil || script.ExitCode != 0 || script.StdoutString() != "go-script-ok" {
		return fmt.Errorf("script failed: result=%+v error=%w", script, err)
	}
	files := sandbox.Files()
	if _, err := files.WriteString(ctx, "/cache/marker.txt", "cache-ok"); err != nil {
		return err
	}
	if value, err := files.ReadString(ctx, "/cache/marker.txt"); err != nil || value != "cache-ok" {
		return fmt.Errorf("named volume cache has unexpected contents: %q: %w", value, err)
	}
	if err := files.MakeDir(ctx, "/workspace/artifacts"); err != nil {
		return err
	}
	if _, err := files.WriteString(ctx, "/workspace/artifacts/result.txt", "hello"); err != nil {
		return err
	}
	if exists, err := files.Exists(ctx, "/workspace/artifacts/result.txt"); err != nil || !exists {
		return fmt.Errorf("written artifact does not exist: %w", err)
	}
	if _, err := files.Stat(ctx, "/workspace/artifacts/result.txt"); err != nil {
		return err
	}
	if entries, err := files.List(ctx, "/workspace/artifacts", 1); err != nil || len(entries) == 0 {
		return fmt.Errorf("artifact directory is empty: %w", err)
	}
	if err := files.Move(ctx, "/workspace/artifacts/result.txt", "/workspace/artifacts/final.txt"); err != nil {
		return err
	}
	if value, err := files.ReadString(ctx, "/workspace/artifacts/final.txt"); err != nil || value != "hello" {
		return fmt.Errorf("moved artifact has unexpected contents: %q: %w", value, err)
	}
	if err := files.Remove(ctx, "/workspace/artifacts"); err != nil {
		return err
	}
	if logs, err := sandbox.Logs(ctx, 20); err != nil || len(logs) > 20 {
		return fmt.Errorf("bounded logs failed: %w", err)
	}
	if stats, err := sandbox.Stats(ctx); err != nil || stats == nil {
		return fmt.Errorf("sandbox stats are unavailable: %w", err)
	}
	if err := sandbox.Pause(ctx, true); err != nil {
		return err
	}
	if running, err := sandbox.IsRunning(ctx); err != nil || running {
		return fmt.Errorf("paused Sandbox reports itself running: %w", err)
	}
	if err := sandbox.Resume(ctx); err != nil {
		return err
	}
	if running, err := sandbox.IsRunning(ctx); err != nil || !running {
		return fmt.Errorf("resumed Sandbox is not running: %w", err)
	}

	if isolation == box.IsolationSandbox {
		if err := exerciseSnapshot(ctx, client, sandbox, image.Reference); err != nil {
			return err
		}
	}
	previousGeneration := sandbox.Generation()
	if err := sandbox.Stop(ctx); err != nil {
		return err
	}
	if running, err := sandbox.IsRunning(ctx); err != nil || running {
		return fmt.Errorf("stopped Sandbox reports itself running: %w", err)
	}
	if err := sandbox.Restart(
		ctx,
		box.RestartOperationID("go-smoke-restart-"+sandbox.ID()),
		box.RestartStopTimeout(5*time.Second),
	); err != nil {
		return err
	}
	if sandbox.Generation() != previousGeneration+1 {
		return errors.New("restart did not advance the Sandbox generation")
	}
	if err := sandbox.Stop(ctx); err != nil {
		return err
	}
	if err := sandbox.Remove(ctx); err != nil {
		return err
	}
	if value, err := client.GetSandbox(ctx, sandbox.ID()); err != nil || value != nil {
		return fmt.Errorf("removed Sandbox remains in inventory: %w", err)
	}

	killProbe := client.Sandbox(image.Reference).Isolation(isolation)
	if isolation == box.IsolationSandbox {
		killProbe = killProbe.Network(box.NoNetwork())
	} else if network != nil {
		killProbe = killProbe.Network(box.BridgeNetwork(network.Name))
	}
	probe, err := killProbe.Start(ctx)
	if err != nil {
		return err
	}
	if err := probe.Close(ctx); err != nil {
		return err
	}
	return nil
}

func exerciseSnapshot(ctx context.Context, client *box.Client, sandbox *box.Sandbox, image string) error {
	marker := "/a3s-go-sdk-snapshot.txt"
	if _, err := sandbox.Files().WriteString(ctx, marker, "snapshot-ok"); err != nil {
		return err
	}
	snapshotID := "go_sdk_" + strings.ReplaceAll(sandbox.ID(), "-", "_")
	snapshot, err := sandbox.CreateFilesystemSnapshot(ctx, snapshotID)
	if err != nil {
		return err
	}
	if size, found, err := client.FilesystemSnapshotSize(ctx, snapshotID); err != nil || !found || size != snapshot.SizeBytes {
		return fmt.Errorf("snapshot size lookup failed: %w", err)
	}
	if snapshots, err := client.ListFilesystemSnapshots(ctx); err != nil || !hasSnapshot(snapshots, snapshotID) {
		return fmt.Errorf("snapshot is absent from inventory: %w", err)
	}
	if value, err := client.GetFilesystemSnapshot(ctx, snapshotID); err != nil || value == nil {
		return fmt.Errorf("snapshot is not gettable: %w", err)
	}
	restored, err := client.Sandbox(image).
		Isolation(box.IsolationSandbox).
		FilesystemSnapshot(snapshotID).
		Network(box.NoNetwork()).
		Start(ctx)
	if err != nil {
		return err
	}
	value, readErr := restored.Files().ReadString(ctx, marker)
	if readErr != nil || value != "snapshot-ok" {
		_ = restored.Close(context.Background())
		return fmt.Errorf("restored snapshot marker is invalid: %q: %w", value, readErr)
	}
	if deleted, deleteErr := client.DeleteFilesystemSnapshot(ctx, snapshotID); deleteErr == nil || deleted {
		_ = restored.Close(context.Background())
		return errors.New("active restored Sandbox did not fence snapshot deletion")
	}
	if err := restored.Close(ctx); err != nil {
		return err
	}
	deleted, err := client.DeleteFilesystemSnapshot(ctx, snapshotID)
	if err != nil || !deleted {
		return fmt.Errorf("snapshot was not deleted after restored Sandbox cleanup: %w", err)
	}
	if _, found, err := client.FilesystemSnapshotSize(ctx, snapshotID); err != nil || found {
		return fmt.Errorf("deleted snapshot still reports a size: %w", err)
	}
	return nil
}

func selectedIsolation() (box.Isolation, error) {
	if len(os.Args) != 2 {
		return "", errors.New("usage: local-sdk-smoke [microvm|sandbox]")
	}
	switch os.Args[1] {
	case string(box.IsolationMicroVM):
		return box.IsolationMicroVM, nil
	case string(box.IsolationSandbox):
		return box.IsolationSandbox, nil
	default:
		return "", fmt.Errorf("unsupported isolation %q", os.Args[1])
	}
}

func hasSandbox(values []box.SandboxSummary, id string) bool {
	return slices.ContainsFunc(values, func(value box.SandboxSummary) bool { return value.ID == id })
}

func hasSnapshot(values []box.FilesystemSnapshotSummary, id string) bool {
	return slices.ContainsFunc(values, func(value box.FilesystemSnapshotSummary) bool { return value.ID == id })
}
