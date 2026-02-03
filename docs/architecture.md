# A3S Box Architecture

## Overview

A3S Box uses a **single-container + file mount** architecture:

1. **Coding Agent Container** - Pluggable coding agents (A3S Code, OpenCode, etc.)
2. **Business Agents** - Mounted as skills via virtio-fs

This design simplifies the architecture, reduces resource overhead, while maintaining flexibility.

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│  Coding Agent Container (VM)                                     │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Coding Agent (A3S Code / OpenCode / Custom)              │  │
│  │  - gRPC Server (4088)                                    │  │
│  │  - Built-in Tools (bash, read, write, edit, grep, glob)  │  │
│  │  - Skill System                                          │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Mounted Business Agents (via virtio-fs)                  │  │
│  │  /a3s/skills/                                            │  │
│  │    ├── order-agent/                                      │  │
│  │    │   ├── SKILL.md          (Skill definition)          │  │
│  │    │   ├── tools/            (Custom tools)              │  │
│  │    │   └── prompts/          (Prompt templates)          │  │
│  │    ├── data-agent/                                       │  │
│  │    └── ...                                               │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

**Architecture Benefits**:
- Only one VM needed, low resource overhead
- No inter-container communication required
- Fast startup time
- Simple configuration
- Seamless integration of business logic with coding capabilities

## Core Concepts

### 1. Coding Agent

The coding agent is the core of A3S Box, responsible for:
- Code generation and editing
- Tool execution
- Skill loading and management
- Session management

Supported coding agent types:
- `a3s_code` - A3S Code (default)
- `opencode` - OpenCode
- `oci_image` - Custom OCI image
- `local_binary` - Local binary
- `remote_binary` - Remote binary

### 2. Business Agent

Business agents are integrated into the coding agent via **file mounting**:

```
Host Filesystem                      Inside Container
/path/to/my-agent/          →    /a3s/skills/my-agent/
  ├── SKILL.md                     ├── SKILL.md
  ├── tools/                       ├── tools/
  │   ├── process_order.py         │   ├── process_order.py
  │   └── validate_data.py         │   └── validate_data.py
  └── prompts/                     └── prompts/
      └── system.md                    └── system.md
```

### 3. Skill System

Business agents run as **Skills**:

```yaml
# SKILL.md
---
name: order-agent
description: Order processing agent
version: 1.0.0
author: MyCompany

# Custom tools
tools:
  - name: process_order
    description: Process an order
    script: tools/process_order.py
    parameters:
      - name: order_id
        type: string
        required: true

  - name: validate_data
    description: Validate data
    script: tools/validate_data.py
    parameters:
      - name: data
        type: object
        required: true

# System prompt
system_prompt: prompts/system.md

# Required built-in tools
requires:
  - bash
  - read_file
  - write_file
---

# Order Agent

This is an order processing agent that can process orders, validate data, etc.
```

## System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     User Application                            │
│                                                                 │
│  from a3s_box import create_box, BoxConfig, AgentConfig        │
│                                                                 │
│  box = await create_box(BoxConfig(                             │
│      coding_agent=AgentConfig(                                 │
│          kind="a3s_code",                                      │
│          llm="/path/to/llm-config.json",                       │
│          skills_dir="/path/to/skills"                          │
│      ),                                                        │
│      skills=[                                                  │
│          SkillMount(name="custom", path="/path/to/custom")     │
│      ]                                                         │
│  ))                                                            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                  Python/TypeScript SDK                          │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Box                                                      │  │
│  │  - generate()        (Code generation)                   │  │
│  │  - use_skill()       (Activate skill)                    │  │
│  │  - remove_skill()    (Remove skill)                      │  │
│  │  - list_skills()     (List skills)                       │  │
│  │  - execute_tool()    (Execute tool)                      │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ gRPC over vsock:4088
┌─────────────────────────────────────────────────────────────────┐
│                    a3s-box-runtime (Rust)                       │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ BoxManager                                               │  │
│  │  - vm: VmManager           (Coding agent VM)             │  │
│  │  - skills_dir_mount        (Skills directory mount)      │  │
│  │  - skill_mounts            (Individual skill mounts)     │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ AgentRegistry                                            │  │
│  │  - Discover and load coding agents                       │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ SkillManager                                             │  │
│  │  - Manage skill mounts (directory + individual)          │  │
│  │  - Load/unload skills                                    │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ virtio-fs
┌─────────────────────────────────────────────────────────────────┐
│                      Coding Agent Container (VM)                 │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Coding Agent (A3S Code / OpenCode / Custom)              │  │
│  │                                                          │  │
│  │  Built-in Tools:                                         │  │
│  │  - bash, read_file, write_file, edit_file               │  │
│  │  - grep, glob, git_status, git_diff, git_commit         │  │
│  │  - web_search, web_fetch, ask_user                      │  │
│  │                                                          │  │
│  │  Skill System:                                           │  │
│  │  - Load skills from /a3s/skills/                        │  │
│  │  - Register custom tools defined in skills              │  │
│  │  - Apply skill system prompts                           │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Mounted Skills Directory (virtio-fs)                     │  │
│  │  /a3s/skills/                                            │  │
│  │    ├── order-agent/        (from skills_dir)            │  │
│  │    ├── data-agent/         (from skills_dir)            │  │
│  │    └── custom/             (from skills list)           │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Workspace (virtio-fs)                                    │  │
│  │  /a3s/workspace/                                         │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## Workspace Crates

| Crate | Type | Purpose |
|-------|------|---------|
| `core` | lib | Foundational types: `BoxConfig`, `BoxError`, `BoxEvent`, `CommandQueue` |
| `runtime` | lib | VM lifecycle, session management, gRPC client, virtio-fs mounts |
| `code` | bin | Guest agent: LLM providers, tool execution, session management |
| `queue` | lib | `QueueManager` (builder pattern) and `QueueMonitor` (health checking) |
| `sdk/python` | cdylib | Python bindings via PyO3 |
| `sdk/typescript` | cdylib | TypeScript bindings via NAPI-RS |

## Skill Lifecycle

### 1. Mounting

When creating a Box, skill directories are mounted into the container via virtio-fs:

```
Host: /path/to/order-agent/
  ↓ virtio-fs
Container: /a3s/skills/order-agent/
```

### 2. Discovery

The coding agent scans `/a3s/skills/` directory to discover available skills:

```python
skills = await box.list_skills()
# ["order-agent", "data-agent", ...]
```

### 3. Activation

When activating a skill, the coding agent:
1. Parses the SKILL.md file
2. Registers custom tools
3. Applies the system prompt
4. Sets environment variables

```python
await box.use_skill("order-agent")
```

### 4. Usage

Once activated, the skill's tools can be called by the LLM:

```python
await box.generate("Process order #12345")
# LLM will call the process_order tool
```

### 5. Deactivation

When deactivating a skill, the coding agent:
1. Removes custom tools
2. Restores the system prompt
3. Cleans up environment variables

```python
await box.remove_skill("order-agent")
```

## Benefits

1. **Simplified Architecture** - Only one VM needed, reduced complexity
2. **Reduced Resources** - Lower memory and CPU usage (~2GB memory, 3s startup)
3. **Fast Startup** - Only one VM to start
4. **Seamless Integration** - Business logic integrates directly with coding capabilities
5. **Easy Development** - Business agents only need SKILL.md and tool scripts
6. **Hot Loading** - Skills can be dynamically loaded/unloaded

## Limitations

1. **Isolation** - Business agents run in the same VM as the coding agent
2. **Resource Sharing** - Business agents share resources with the coding agent
3. **Language Constraints** - Custom tools must be executable scripts

---

**Last Updated**: 2026-02-04
