package box

import (
	"context"
	"encoding/base64"
	"errors"
	"strings"
)

type FileOption interface{ applyFile(*fileConfig) }
type fileOptionFunc func(*fileConfig)

func (option fileOptionFunc) applyFile(config *fileConfig) { option(config) }

type fileConfig struct {
	user string
}

func FileAs(user string) FileOption {
	return fileOptionFunc(func(config *fileConfig) { config.user = user })
}

type Filesystem struct {
	sandbox *Sandbox
}

func (sandbox *Sandbox) Files() *Filesystem { return &Filesystem{sandbox: sandbox} }

func (filesystem *Filesystem) Write(
	ctx context.Context,
	path string,
	data []byte,
	options ...FileOption,
) (WriteInfo, error) {
	const op = "file_write"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return WriteInfo{}, err
	}
	fields["data_base64"] = base64.StdEncoding.EncodeToString(data)
	var result WriteInfo
	err = filesystem.sandbox.readRequest(ctx, op, fields, &result, true)
	return result, err
}

func (filesystem *Filesystem) WriteString(
	ctx context.Context,
	path, data string,
	options ...FileOption,
) (WriteInfo, error) {
	return filesystem.Write(ctx, path, []byte(data), options...)
}

func (filesystem *Filesystem) Read(
	ctx context.Context,
	path string,
	options ...FileOption,
) ([]byte, error) {
	const op = "file_read"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return nil, err
	}
	var result struct {
		DataBase64 string `json:"data_base64"`
	}
	if err := filesystem.sandbox.readRequest(ctx, op, fields, &result, true); err != nil {
		return nil, err
	}
	data, err := base64.StdEncoding.DecodeString(result.DataBase64)
	if err != nil {
		return nil, sdkError(op, CodeProtocol, "file contents are not valid base64", err)
	}
	return data, nil
}

func (filesystem *Filesystem) ReadString(
	ctx context.Context,
	path string,
	options ...FileOption,
) (string, error) {
	data, err := filesystem.Read(ctx, path, options...)
	return string(data), err
}

func (filesystem *Filesystem) Stat(
	ctx context.Context,
	path string,
	options ...FileOption,
) (EntryInfo, error) {
	const op = "filesystem_stat"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return EntryInfo{}, err
	}
	var result struct {
		Entry EntryInfo `json:"entry"`
	}
	err = filesystem.sandbox.readRequest(ctx, op, fields, &result, true)
	return result.Entry, err
}

func (filesystem *Filesystem) Exists(
	ctx context.Context,
	path string,
	options ...FileOption,
) (bool, error) {
	_, err := filesystem.Stat(ctx, path, options...)
	if errors.Is(err, ErrNotFound) {
		return false, nil
	}
	return err == nil, err
}

func (filesystem *Filesystem) List(
	ctx context.Context,
	path string,
	depth uint32,
	options ...FileOption,
) ([]EntryInfo, error) {
	const op = "filesystem_list"
	if depth == 0 {
		return nil, invalid(op, "filesystem list depth must be greater than zero")
	}
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return nil, err
	}
	fields["depth"] = depth
	var result struct {
		Entries []EntryInfo `json:"entries"`
	}
	if err := filesystem.sandbox.readRequest(ctx, op, fields, &result, true); err != nil {
		return nil, err
	}
	return result.Entries, nil
}

func (filesystem *Filesystem) MakeDir(
	ctx context.Context,
	path string,
	options ...FileOption,
) error {
	const op = "filesystem_make_dir"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return err
	}
	return filesystem.sandbox.readRequest(ctx, op, fields, &struct{}{}, true)
}

func (filesystem *Filesystem) Move(
	ctx context.Context,
	path, destination string,
	options ...FileOption,
) error {
	const op = "filesystem_move"
	if strings.TrimSpace(destination) == "" {
		return invalid(op, "filesystem destination cannot be empty")
	}
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return err
	}
	fields["destination"] = destination
	return filesystem.sandbox.readRequest(ctx, op, fields, &struct{}{}, true)
}

func (filesystem *Filesystem) Remove(
	ctx context.Context,
	path string,
	options ...FileOption,
) error {
	const op = "filesystem_remove"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return err
	}
	return filesystem.sandbox.readRequest(ctx, op, fields, &struct{}{}, true)
}

func (filesystem *Filesystem) fields(
	operation, path string,
	options []FileOption,
) (map[string]any, error) {
	if filesystem == nil || filesystem.sandbox == nil {
		return nil, invalid(operation, "filesystem handle is not initialized")
	}
	if strings.TrimSpace(path) == "" {
		return nil, invalid(operation, "filesystem path cannot be empty")
	}
	config := fileConfig{}
	for _, option := range options {
		if option != nil {
			option.applyFile(&config)
		}
	}
	if config.user != "" && strings.TrimSpace(config.user) == "" {
		return nil, invalid(operation, "filesystem user cannot be blank")
	}
	fields := map[string]any{"path": path}
	if config.user != "" {
		fields["user"] = config.user
	}
	return fields, nil
}
