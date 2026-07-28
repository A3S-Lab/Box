package box

import (
	"context"
	"encoding/base64"
	"strings"
	"time"
)

// Command is an explicit argv command. Use Shell only when shell semantics are
// intentionally required.
type Command struct {
	argv []string
}

func Argv(arguments ...string) Command {
	return Command{argv: append([]string(nil), arguments...)}
}

func Shell(source string) Command {
	return Argv("/bin/sh", "-lc", source)
}

func (command Command) Arguments() []string {
	return append([]string(nil), command.argv...)
}

type RunOption interface{ applyRun(*runConfig) }
type runOptionFunc func(*runConfig)

func (option runOptionFunc) applyRun(config *runConfig) { option(config) }

type runConfig struct {
	timeout *time.Duration
	env     map[string]string
	cwd     string
	user    string
	stdin   *[]byte
}

func RunTimeout(timeout time.Duration) RunOption {
	return runOptionFunc(func(config *runConfig) { config.timeout = &timeout })
}

func RunEnv(key, value string) RunOption {
	return runOptionFunc(func(config *runConfig) { config.env[key] = value })
}

func RunDirectory(path string) RunOption {
	return runOptionFunc(func(config *runConfig) { config.cwd = path })
}

func RunAs(user string) RunOption {
	return runOptionFunc(func(config *runConfig) { config.user = user })
}

func RunStdin(data []byte) RunOption {
	return runOptionFunc(func(config *runConfig) {
		copy := append([]byte(nil), data...)
		config.stdin = &copy
	})
}

func RunStdinString(data string) RunOption { return RunStdin([]byte(data)) }

type Commands struct {
	sandbox *Sandbox
}

func (sandbox *Sandbox) Commands() *Commands { return &Commands{sandbox: sandbox} }

func (sandbox *Sandbox) Run(
	ctx context.Context,
	command Command,
	options ...RunOption,
) (CommandResult, error) {
	return sandbox.Commands().Run(ctx, command, options...)
}

func (commands *Commands) Run(
	ctx context.Context,
	command Command,
	options ...RunOption,
) (CommandResult, error) {
	const op = "command_run"
	if commands == nil || commands.sandbox == nil {
		return CommandResult{}, invalid(op, "commands handle is not initialized")
	}
	if len(command.argv) == 0 {
		return CommandResult{}, invalid(op, "command cannot be empty")
	}
	for _, argument := range command.argv {
		if strings.IndexByte(argument, 0) >= 0 {
			return CommandResult{}, invalid(op, "command arguments cannot contain NUL bytes")
		}
	}
	config := runConfig{env: make(map[string]string)}
	for _, option := range options {
		if option != nil {
			option.applyRun(&config)
		}
	}
	if config.timeout != nil && *config.timeout <= 0 {
		return CommandResult{}, invalid(op, "command timeout must be greater than zero")
	}
	if config.cwd != "" && strings.TrimSpace(config.cwd) == "" {
		return CommandResult{}, invalid(op, "command working directory cannot be blank")
	}
	if config.user != "" && strings.TrimSpace(config.user) == "" {
		return CommandResult{}, invalid(op, "command user cannot be blank")
	}
	for key := range config.env {
		if strings.TrimSpace(key) == "" {
			return CommandResult{}, invalid(op, "environment variable name cannot be empty")
		}
	}
	fields := map[string]any{
		"argv": append([]string(nil), command.argv...),
		"env":  cloneStringMap(config.env),
	}
	if config.timeout != nil {
		fields["timeout_ms"] = durationMilliseconds(*config.timeout)
	}
	if config.cwd != "" {
		fields["cwd"] = config.cwd
	}
	if config.user != "" {
		fields["user"] = config.user
	}
	if config.stdin != nil {
		fields["stdin_base64"] = base64.StdEncoding.EncodeToString(*config.stdin)
	}
	var wire struct {
		StdoutBase64 string `json:"stdout_base64"`
		StderrBase64 string `json:"stderr_base64"`
		ExitCode     int    `json:"exit_code"`
		Truncated    bool   `json:"truncated"`
	}
	if err := commands.sandbox.readRequest(ctx, op, fields, &wire, true); err != nil {
		return CommandResult{}, err
	}
	stdout, err := base64.StdEncoding.DecodeString(wire.StdoutBase64)
	if err != nil {
		return CommandResult{}, sdkError(op, CodeProtocol, "command stdout is not valid base64", err)
	}
	stderr, err := base64.StdEncoding.DecodeString(wire.StderrBase64)
	if err != nil {
		return CommandResult{}, sdkError(op, CodeProtocol, "command stderr is not valid base64", err)
	}
	return CommandResult{
		Stdout:    stdout,
		Stderr:    stderr,
		ExitCode:  wire.ExitCode,
		Truncated: wire.Truncated,
	}, nil
}

func durationMilliseconds(duration time.Duration) uint64 {
	milliseconds := duration / time.Millisecond
	if duration%time.Millisecond != 0 {
		milliseconds++
	}
	return uint64(milliseconds)
}
