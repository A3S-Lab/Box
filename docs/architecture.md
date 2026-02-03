# A3S Box 架构设计

## 概述

A3S Box 采用**单容器 + 文件挂载**架构：

1. **编码智能体容器** - 可插拔的编码智能体（A3S Code、OpenCode 等）
2. **业务智能体** - 通过文件挂载到容器中，作为技能（Skill）运行

这种设计简化了架构，减少了资源开销，同时保持了灵活性。

## 架构图

```
┌─────────────────────────────────────────────────────────────────┐
│  编码智能体容器 (VM)                                              │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Coding Agent (A3S Code / OpenCode / Custom)              │  │
│  │  - gRPC Server (4088)                                    │  │
│  │  - 内置工具 (bash, read, write, edit, grep, glob, git)   │  │
│  │  - 技能系统 (Skill System)                               │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ 挂载的业务智能体 (通过 virtio-fs)                          │  │
│  │  /a3s/skills/                                            │  │
│  │    ├── order-agent/                                      │  │
│  │    │   ├── SKILL.md          (技能定义)                   │  │
│  │    │   ├── tools/            (自定义工具)                 │  │
│  │    │   └── prompts/          (提示词模板)                 │  │
│  │    ├── data-agent/                                       │  │
│  │    └── ...                                               │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

**架构优势**：
- ✅ 只需要一个 VM，资源开销小
- ✅ 无需容器间通信
- ✅ 启动时间短
- ✅ 配置简单
- ✅ 业务逻辑与编码能力无缝集成

## 核心概念

### 1. 编码智能体（Coding Agent）

编码智能体是 A3S Box 的核心，负责：
- 代码生成和编辑
- 工具执行
- 技能加载和管理
- 会话管理

支持的编码智能体类型：
- `a3s_code` - A3S Code（默认）
- `opencode` - OpenCode
- `oci_image` - 自定义 OCI 镜像
- `local_binary` - 本地二进制
- `remote_binary` - 远程二进制

### 2. 业务智能体（Business Agent）

业务智能体通过**文件挂载**的方式集成到编码智能体中：

```
主机文件系统                      容器内部
/path/to/my-agent/          →    /a3s/skills/my-agent/
  ├── SKILL.md                     ├── SKILL.md
  ├── tools/                       ├── tools/
  │   ├── process_order.py         │   ├── process_order.py
  │   └── validate_data.py         │   └── validate_data.py
  └── prompts/                     └── prompts/
      └── system.md                    └── system.md
```

### 3. 技能系统（Skill System）

业务智能体以**技能（Skill）**的形式运行：

```yaml
# SKILL.md
---
name: order-agent
description: 订单处理智能体
version: 1.0.0
author: MyCompany

# 自定义工具
tools:
  - name: process_order
    description: 处理订单
    script: tools/process_order.py
    parameters:
      - name: order_id
        type: string
        required: true

  - name: validate_data
    description: 验证数据
    script: tools/validate_data.py
    parameters:
      - name: data
        type: object
        required: true

# 系统提示词
system_prompt: prompts/system.md

# 依赖的内置工具
requires:
  - bash
  - read_file
  - write_file
---

# Order Agent

这是一个订单处理智能体，可以处理订单、验证数据等。

## 使用方法

激活此技能后，你可以使用以下工具：
- `process_order`: 处理订单
- `validate_data`: 验证数据
```

## 架构图

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
│          skills_dir="/path/to/skills"  # 技能目录挂载           │
│      ),                                                        │
│      skills=[                          # 额外单独挂载           │
│          SkillMount(name="custom", path="/path/to/custom")     │
│      ]                                                         │
│  ))                                                            │
│                                                                 │
│  # 使用编码智能体                                                │
│  await box.generate("Write a function...")                    │
│                                                                 │
│  # 激活业务技能                                                  │
│  await box.use_skill("order-agent")                           │
│  await box.generate("Process order #12345")                   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                  Python/TypeScript SDK                          │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Box                                                      │  │
│  │  - generate()        (代码生成)                          │  │
│  │  - use_skill()       (激活技能)                          │  │
│  │  - remove_skill()    (移除技能)                          │  │
│  │  - list_skills()     (列出技能)                          │  │
│  │  - execute_tool()    (执行工具)                          │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ gRPC over vsock:4088
┌─────────────────────────────────────────────────────────────────┐
│                    a3s-box-runtime (Rust)                       │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ BoxManager                                               │  │
│  │  - vm: VmManager           (编码智能体 VM)               │  │
│  │  - skills_dir_mount: Option<String>  (技能目录挂载)      │  │
│  │  - skill_mounts: Vec<SkillMount>     (单独技能挂载)      │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ AgentRegistry                                            │  │
│  │  - 发现和加载编码智能体                                    │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ SkillManager                                             │  │
│  │  - 管理技能挂载（目录 + 单独）                             │  │
│  │  - 加载/卸载技能                                          │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ virtio-fs
┌─────────────────────────────────────────────────────────────────┐
│                      编码智能体容器 (VM)                          │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Coding Agent (A3S Code / OpenCode / Custom)              │  │
│  │                                                          │  │
│  │  内置工具:                                                │  │
│  │  - bash, read_file, write_file, edit_file               │  │
│  │  - grep, glob, git_status, git_diff, git_commit         │  │
│  │  - web_search, web_fetch, ask_user                      │  │
│  │                                                          │  │
│  │  技能系统:                                                │  │
│  │  - 加载 /a3s/skills/ 下的技能                            │  │
│  │  - 注册技能定义的自定义工具                               │  │
│  │  - 应用技能的系统提示词                                   │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ 挂载的技能目录 (virtio-fs)                                │  │
│  │                                                          │  │
│  │  /a3s/skills/                                            │  │
│  │    ├── order-agent/        (从 skills_dir 挂载)         │  │
│  │    │   ├── SKILL.md                                      │  │
│  │    │   ├── tools/                                        │  │
│  │    │   │   ├── process_order.py                         │  │
│  │    │   │   └── validate_data.py                         │  │
│  │    │   └── prompts/                                      │  │
│  │    │       └── system.md                                 │  │
│  │    │                                                     │  │
│  │    ├── data-agent/         (从 skills_dir 挂载)         │  │
│  │    │   ├── SKILL.md                                      │  │
│  │    │   └── tools/                                        │  │
│  │    │       └── analyze.py                               │  │
│  │    │                                                     │  │
│  │    ├── custom/             (从 skills 单独挂载)          │  │
│  │    │   ├── SKILL.md                                      │  │
│  │    │   └── tools/                                        │  │
│  │    │                                                     │  │
│  │    └── ...                                               │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ 工作目录 (virtio-fs)                                      │  │
│  │  /a3s/workspace/                                         │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## 配置结构

### BoxConfig

```python
@dataclass
class BoxConfig:
    """A3S Box 配置"""

    # Box ID（可选，不提供则自动生成）
    box_id: Optional[str] = None

    # 编码智能体配置（默认：a3s-code）
    coding_agent: AgentConfig = field(default_factory=lambda: AgentConfig(kind="a3s_code"))

    # 技能挂载列表（业务智能体）
    skills: List[SkillMount] = field(default_factory=list)

    # 资源配置
    resources: ResourceConfig = field(default_factory=ResourceConfig)

    # 网络配置
    network: NetworkConfig = field(default_factory=NetworkConfig)

    # 工作目录
    workspace: str = "/workspace"
```

### SkillMount

```python
@dataclass
class SkillMount:
    """技能挂载配置"""

    # 技能名称
    name: str

    # 主机上的技能目录路径
    path: str

    # 是否自动激活
    auto_activate: bool = False

    # 环境变量
    env: Dict[str, str] = field(default_factory=dict)
```

### AgentConfig

```python
@dataclass
class AgentConfig:
    """编码智能体配置"""

    # 智能体类型
    kind: str  # "a3s_code", "opencode", "oci_image", "local_binary", "remote_binary"

    # 版本（可选）
    version: Optional[str] = None

    # OCI 镜像（kind="oci_image" 时必需）
    image: Optional[str] = None

    # 本地二进制路径（kind="local_binary" 时必需）
    path: Optional[str] = None

    # 远程二进制 URL（kind="remote_binary" 时必需）
    url: Optional[str] = None

    # 校验和（kind="remote_binary" 时必需）
    checksum: Optional[str] = None

    # 自定义入口点
    entrypoint: Optional[str] = None

    # 环境变量
    env: Dict[str, str] = field(default_factory=dict)

    # LLM 配置（支持对象或文件路径）
    llm: Optional[Union[LLMConfig, str]] = None

    # Skills 配置目录（挂载到容器内 /a3s/skills/）
    skills_dir: Optional[str] = None
```

### SkillsConfig

Skills 配置支持两种方式：

1. **目录挂载** - 通过 `AgentConfig.skills_dir` 指定本地目录，整个目录挂载到容器内
2. **单独挂载** - 通过 `BoxConfig.skills` 列表单独挂载每个技能

```python
# 方式 1: 目录挂载（推荐用于多个技能）
# 主机: /path/to/skills/
#   ├── order-agent/
#   │   └── SKILL.md
#   └── data-agent/
#       └── SKILL.md
# 容器: /a3s/skills/
#   ├── order-agent/
#   └── data-agent/

box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        skills_dir="/path/to/skills"  # 整个目录挂载
    )
))

# 方式 2: 单独挂载（推荐用于分散的技能）
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(kind="a3s_code"),
    skills=[
        SkillMount(name="order-agent", path="/path/to/order-agent"),
        SkillMount(name="data-agent", path="/other/path/data-agent"),
    ]
))

# 方式 3: 混合使用
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        skills_dir="/path/to/common-skills"  # 通用技能目录
    ),
    skills=[
        SkillMount(name="custom-agent", path="/path/to/custom-agent"),  # 额外技能
    ]
))
```

### LLMConfig

```python
@dataclass
class LLMConfig:
    """LLM 配置"""

    # 默认提供商
    default_provider: str

    # 默认模型
    default_model: str

    # 提供商列表
    providers: List[ProviderConfig]

    @classmethod
    def from_file(cls, path: str) -> "LLMConfig":
        """从文件加载配置"""
        with open(path) as f:
            data = json.load(f)
        return cls.from_dict(data)

    @classmethod
    def from_dict(cls, data: dict) -> "LLMConfig":
        """从字典创建配置"""
        return cls(
            default_provider=data["defaultProvider"],
            default_model=data["defaultModel"],
            providers=[ProviderConfig.from_dict(p) for p in data["providers"]]
        )

@dataclass
class ProviderConfig:
    """LLM 提供商配置"""

    # 提供商名称
    name: str

    # API Key
    api_key: str

    # Base URL
    base_url: str

    # 模型列表
    models: List[ModelConfig]

    @classmethod
    def from_dict(cls, data: dict) -> "ProviderConfig":
        return cls(
            name=data["name"],
            api_key=data["apiKey"],
            base_url=data["baseUrl"],
            models=[ModelConfig.from_dict(m) for m in data["models"]]
        )

@dataclass
class ModelConfig:
    """模型配置"""

    # 模型 ID
    id: str

    # 模型名称
    name: str

    # 模型家族
    family: str

    # 支持附件
    attachment: bool

    # 支持推理
    reasoning: bool

    # 支持工具调用
    tool_call: bool

    # 支持温度参数
    temperature: bool

    # 发布日期（可选）
    release_date: Optional[str] = None

    # 模态支持
    modalities: Optional[dict] = None

    # 成本信息（可选）
    cost: Optional[dict] = None

    # 限制信息
    limit: Optional[dict] = None

    @classmethod
    def from_dict(cls, data: dict) -> "ModelConfig":
        return cls(
            id=data["id"],
            name=data["name"],
            family=data["family"],
            attachment=data["attachment"],
            reasoning=data["reasoning"],
            tool_call=data["toolCall"],
            temperature=data["temperature"],
            release_date=data.get("releaseDate"),
            modalities=data.get("modalities"),
            cost=data.get("cost"),
            limit=data.get("limit")
        )
```

## 使用示例

### 示例 1: 默认配置（仅编码智能体）

```python
from a3s_box import create_box

# 使用默认的 A3S Code，无业务智能体
box = await create_box()

# 使用编码智能体
await box.generate("Write a Python function to calculate fibonacci")
```

### 示例 2: 挂载业务智能体

```python
from a3s_box import create_box, BoxConfig, SkillMount

box = await create_box(BoxConfig(
    skills=[
        SkillMount(
            name="order-agent",
            path="/path/to/order-agent",
            auto_activate=True,  # 自动激活
        )
    ]
))

# 业务智能体已自动激活，可以直接使用其工具
await box.generate("Process order #12345")
```

### 示例 3: 手动激活技能

```python
box = await create_box(BoxConfig(
    skills=[
        SkillMount(name="order-agent", path="/path/to/order-agent"),
        SkillMount(name="data-agent", path="/path/to/data-agent"),
    ]
))

# 列出可用技能
skills = await box.list_skills()
print(skills)  # ["order-agent", "data-agent"]

# 激活订单处理技能
await box.use_skill("order-agent")
await box.generate("Process order #12345")

# 切换到数据分析技能
await box.remove_skill("order-agent")
await box.use_skill("data-agent")
await box.generate("Analyze sales data")
```

### 示例 4: 使用 OpenCode + 业务智能体

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="opencode",
        # 方式 1: 从配置文件加载 LLM 配置
        llm="/path/to/llm-config.json"
    ),
    skills=[
        SkillMount(
            name="order-agent",
            path="/path/to/order-agent",
            env={
                "DATABASE_URL": os.getenv("DATABASE_URL"),
            }
        )
    ]
))
```

### 示例 5: 内联 LLM 配置

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        # 方式 2: 内联 LLM 配置对象
        llm=LLMConfig(
            default_provider="anthropic",
            default_model="claude-sonnet-4-20250514",
            providers=[
                ProviderConfig(
                    name="anthropic",
                    api_key=os.getenv("ANTHROPIC_API_KEY"),
                    base_url="https://api.anthropic.com/v1",
                    models=[
                        ModelConfig(
                            id="claude-sonnet-4-20250514",
                            name="Claude Sonnet 4",
                            family="claude-sonnet",
                            attachment=True,
                            reasoning=False,
                            tool_call=True,
                            temperature=True,
                            modalities={
                                "input": ["text", "image", "pdf"],
                                "output": ["text"]
                            },
                            cost={"input": 3, "output": 15},
                            limit={"context": 200000, "output": 64000}
                        )
                    ]
                ),
                ProviderConfig(
                    name="kimi",
                    api_key=os.getenv("KIMI_API_KEY"),
                    base_url="http://35.220.164.252:3888/v1",
                    models=[
                        ModelConfig(
                            id="kimi-k2-5",
                            name="KIMI K2.5",
                            family="kimi",
                            attachment=False,
                            reasoning=False,
                            tool_call=True,
                            temperature=True,
                            limit={"context": 128000, "output": 4096}
                        )
                    ]
                )
            ]
        )
    )
))
```

### 示例 6: 从 YAML 配置

```python
config = BoxConfig.from_yaml("box-config.yaml")
box = await create_box(config)
```

**box-config.yaml**:
```yaml
coding_agent:
  kind: "a3s_code"
  version: "0.1.0"
  llm: "./llm-config.json"  # 或内联配置

skills:
  - name: "order-agent"
    path: "./skills/order-agent"
    auto_activate: true
    env:
      DATABASE_URL: "${DATABASE_URL}"

  - name: "data-agent"
    path: "./skills/data-agent"
    auto_activate: false

resources:
  memory: 2147483648  # 2GB
  cpus: 2
  disk: 10737418240   # 10GB

network:
  enable_external: true

workspace: "./workspace"
```

**llm-config.json**:
```json
{
  "defaultProvider": "kimi",
  "defaultModel": "kimi-k2-5",
  "providers": [
    {
      "name": "anthropic",
      "apiKey": "${ANTHROPIC_API_KEY}",
      "baseUrl": "https://api.anthropic.com/v1",
      "models": [
        {
          "id": "claude-sonnet-4-20250514",
          "name": "Claude Sonnet 4",
          "family": "claude-sonnet",
          "attachment": true,
          "reasoning": false,
          "toolCall": true,
          "temperature": true,
          "modalities": {
            "input": ["text", "image", "pdf"],
            "output": ["text"]
          },
          "cost": {
            "input": 3,
            "output": 15,
            "cacheRead": 0.3,
            "cacheWrite": 3.75
          },
          "limit": {
            "context": 200000,
            "output": 64000
          }
        }
      ]
    },
    {
      "name": "kimi",
      "apiKey": "${KIMI_API_KEY}",
      "baseUrl": "http://35.220.164.252:3888/v1",
      "models": [
        {
          "id": "kimi-k2-5",
          "name": "KIMI K2.5",
          "family": "kimi",
          "attachment": false,
          "reasoning": false,
          "toolCall": true,
          "temperature": true,
          "limit": {
            "context": 128000,
            "output": 4096
          }
        }
      ]
    }
  ]
}
```

## 业务智能体开发指南

### 目录结构

```
my-business-agent/
├── SKILL.md              # 技能定义（必需）
├── tools/                # 自定义工具目录
│   ├── tool1.py
│   ├── tool2.py
│   └── tool3.sh
├── prompts/              # 提示词模板目录
│   ├── system.md         # 系统提示词
│   └── examples.md       # 示例
└── data/                 # 数据目录（可选）
    └── config.json
```

### SKILL.md 格式

```yaml
---
# 元数据
name: my-business-agent
description: 我的业务智能体
version: 1.0.0
author: MyCompany
license: MIT

# 自定义工具定义
tools:
  - name: my_tool
    description: 我的工具描述
    script: tools/my_tool.py
    interpreter: python3  # 可选，默认根据扩展名推断
    timeout: 30           # 超时时间（秒）
    parameters:
      - name: param1
        type: string
        description: 参数1描述
        required: true
      - name: param2
        type: integer
        description: 参数2描述
        required: false
        default: 10

# 系统提示词
system_prompt: |
  你是一个专业的业务处理助手。

  ## 你的能力
  - 处理订单
  - 验证数据
  - 生成报告

# 或者引用外部文件
# system_prompt: prompts/system.md

# 依赖的内置工具
requires:
  - bash
  - read_file
  - write_file

# 环境变量要求
env_requires:
  - DATABASE_URL
  - API_KEY
---

# My Business Agent

这是我的业务智能体的详细说明。

## 功能

1. 处理订单
2. 验证数据
3. 生成报告

## 使用示例

```
请处理订单 #12345
```
```

### 自定义工具示例

**tools/process_order.py**:
```python
#!/usr/bin/env python3
"""
处理订单工具

参数:
  - order_id: 订单ID (string, required)
  - action: 操作类型 (string, optional, default="process")

返回:
  JSON 格式的处理结果
"""

import json
import sys
import os

def main():
    # 从命令行参数获取输入
    if len(sys.argv) < 2:
        print(json.dumps({"error": "Missing order_id"}))
        sys.exit(1)

    order_id = sys.argv[1]
    action = sys.argv[2] if len(sys.argv) > 2 else "process"

    # 从环境变量获取配置
    database_url = os.getenv("DATABASE_URL")

    # 处理订单逻辑
    result = {
        "order_id": order_id,
        "action": action,
        "status": "success",
        "message": f"Order {order_id} processed successfully"
    }

    # 输出 JSON 结果
    print(json.dumps(result))

if __name__ == "__main__":
    main()
```

**tools/validate_data.sh**:
```bash
#!/bin/bash
# 验证数据工具
#
# 参数:
#   $1 - 数据文件路径

DATA_FILE="$1"

if [ ! -f "$DATA_FILE" ]; then
    echo '{"error": "File not found"}'
    exit 1
fi

# 验证逻辑
if jq empty "$DATA_FILE" 2>/dev/null; then
    echo '{"valid": true, "message": "Data is valid JSON"}'
else
    echo '{"valid": false, "message": "Invalid JSON format"}'
fi
```

## 技能生命周期

### 1. 挂载

当创建 Box 时，技能目录通过 virtio-fs 挂载到容器中：

```
主机: /path/to/order-agent/
  ↓ virtio-fs
容器: /a3s/skills/order-agent/
```

### 2. 发现

编码智能体扫描 `/a3s/skills/` 目录，发现所有可用技能：

```python
skills = await box.list_skills()
# ["order-agent", "data-agent", ...]
```

### 3. 激活

激活技能时，编码智能体：
1. 解析 SKILL.md 文件
2. 注册自定义工具
3. 应用系统提示词
4. 设置环境变量

```python
await box.use_skill("order-agent")
```

### 4. 使用

激活后，技能的工具可以被 LLM 调用：

```python
await box.generate("Process order #12345")
# LLM 会调用 process_order 工具
```

### 5. 卸载

卸载技能时，编码智能体：
1. 移除自定义工具
2. 恢复系统提示词
3. 清理环境变量

```python
await box.remove_skill("order-agent")
```

## 优势

1. **简化架构** - 只需要一个 VM，减少复杂性
2. **减少资源** - 内存和 CPU 占用更少（约 2GB 内存，3 秒启动）
3. **快速启动** - 只需启动一个 VM
4. **无缝集成** - 业务逻辑与编码能力直接集成
5. **易于开发** - 业务智能体只需编写 SKILL.md 和工具脚本
6. **热加载** - 可以动态加载/卸载技能

## 限制

1. **隔离性** - 业务智能体与编码智能体在同一 VM 中运行
2. **资源共享** - 业务智能体与编码智能体共享资源
3. **语言限制** - 自定义工具需要是可执行脚本

---

**最后更新**: 2026-02-03
