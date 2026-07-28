package box

import (
	"context"
	"strings"
	"time"
)

// ScriptBuilder executes source through stdin, so scripts never require a
// temporary host file and their contents do not appear in process arguments.
type ScriptBuilder struct {
	commands    *Commands
	source      []byte
	interpreter Command
	timeout     *time.Duration
	env         map[string]string
	cwd         string
	user        string
}

func (sandbox *Sandbox) Script(source string) *ScriptBuilder {
	return sandbox.Commands().Script(source)
}

func (sandbox *Sandbox) ScriptBytes(source []byte) *ScriptBuilder {
	return sandbox.Commands().ScriptBytes(source)
}

func (commands *Commands) Script(source string) *ScriptBuilder {
	return commands.ScriptBytes([]byte(source))
}

func (commands *Commands) ScriptBytes(source []byte) *ScriptBuilder {
	return &ScriptBuilder{
		commands:    commands,
		source:      append([]byte(nil), source...),
		interpreter: Argv("/bin/sh", "-se"),
		env:         make(map[string]string),
	}
}

func (builder *ScriptBuilder) Interpreter(executable string, arguments ...string) *ScriptBuilder {
	builder.interpreter = Argv(append([]string{executable}, arguments...)...)
	return builder
}

func (builder *ScriptBuilder) Timeout(timeout time.Duration) *ScriptBuilder {
	builder.timeout = &timeout
	return builder
}

func (builder *ScriptBuilder) Env(key, value string) *ScriptBuilder {
	builder.env[key] = value
	return builder
}

func (builder *ScriptBuilder) Directory(path string) *ScriptBuilder {
	builder.cwd = path
	return builder
}

func (builder *ScriptBuilder) User(user string) *ScriptBuilder {
	builder.user = user
	return builder
}

func (builder *ScriptBuilder) Run(ctx context.Context) (CommandResult, error) {
	const op = "command_run"
	if builder == nil || builder.commands == nil {
		return CommandResult{}, invalid(op, "script builder is not initialized")
	}
	if len(builder.source) == 0 {
		return CommandResult{}, invalid(op, "script source cannot be empty")
	}
	if len(builder.interpreter.argv) == 0 || strings.TrimSpace(builder.interpreter.argv[0]) == "" {
		return CommandResult{}, invalid(op, "script interpreter cannot be empty")
	}
	options := make([]RunOption, 0, len(builder.env)+4)
	if builder.timeout != nil {
		options = append(options, RunTimeout(*builder.timeout))
	}
	for key, value := range builder.env {
		options = append(options, RunEnv(key, value))
	}
	if builder.cwd != "" {
		options = append(options, RunDirectory(builder.cwd))
	}
	if builder.user != "" {
		options = append(options, RunAs(builder.user))
	}
	options = append(options, RunStdin(builder.source))
	return builder.commands.Run(ctx, builder.interpreter, options...)
}
