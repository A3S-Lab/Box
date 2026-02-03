# A3S Box 配置指南

## 概述

本文档详细说明如何配置 A3S Box 实例，包括编码智能体类型、业务智能体、资源限制等。

## 快速开始

### 默认配置（使用 A3S Code）

```python
from a3s_box import create_box

# 使用默认配置：A3S Code 作为编码智能体
box = await create_box()

# 使用编码智能体
await box.coding.generate("Write a Python function to calculate fibonacci")
```

### 指定编码智能体类型

```python
from a3s_box import create_box, BoxConfig, AgentConfig

# 使用 OpenCode 作为编码智能体
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(kind="opencode")
))

# 使用自定义编码智能体
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="oci_image",
        image="ghcr.io/myorg/my-coding-agent:v1"
    )
))
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

### AgentConfig

```python
@dataclass
class AgentConfig:
    """智能体配置"""

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
```

### ResourceConfig

```python
@dataclass
class ResourceConfig:
    """资源配置"""

    # 内存限制
    memory: int = 2 * 1024 * 1024 * 1024  # 2GB

    # CPU 核心数
    cpus: int = 2

    # 磁盘限制
    disk: int = 10 * 1024 * 1024 * 1024  # 10GB
```

### NetworkConfig

```python
@dataclass
class NetworkConfig:
    """网络配置"""

    # 是否启用外部网络访问
    enable_external: bool = True
```

## 使用示例

### 示例 1: 默认配置（A3S Code）

```python
from a3s_box import create_box

# 最简单的方式：使用所有默认值
box = await create_box()

# 等价于：
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(kind="a3s_code"),
    skills=[],
    resources=ResourceConfig(),
    network=NetworkConfig(),
))
```

### 示例 2: 使用 OpenCode + LLM 配置文件

```python
from a3s_box import create_box, BoxConfig, AgentConfig

box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="opencode",
        version="latest",
        # 从文件加载 LLM 配置
        llm="/path/to/llm-config.json"
    )
))
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

### 示例 3: 使用特定版本的 A3S Code + 内联 LLM 配置

```python
from a3s_box import create_box, BoxConfig, AgentConfig, LLMConfig, ProviderConfig, ModelConfig

box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        version="0.2.0",
        # 内联 LLM 配置对象
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
                )
            ]
        )
    )
))
```

### 示例 4: 使用自定义 OCI 镜像

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="oci_image",
        image="ghcr.io/myorg/my-coding-agent:v1.0.0",
        entrypoint="exec /app/agent --port 4088",
        env={
            "RUST_LOG": "debug",
            "ANTHROPIC_API_KEY": os.getenv("ANTHROPIC_API_KEY"),
        }
    )
))
```

### 示例 5: 使用本地二进制

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="local_binary",
        path="/path/to/my-agent",
        entrypoint="exec /a3s/agent/my-agent --listen vsock://4088",
        env={
            "WORKSPACE": "/workspace",
        }
    )
))
```

### 示例 6: 使用远程二进制

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="remote_binary",
        url="https://releases.example.com/agent-v1.0.tar.gz",
        checksum="sha256:abc123...",
    )
))
```

### 示例 7: Skills 目录挂载

```python
# 方式 1: 通过 skills_dir 挂载整个目录
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        llm="/path/to/llm-config.json",
        skills_dir="/path/to/skills"  # 整个目录挂载到 /a3s/skills/
    )
))

# 主机目录结构:
# /path/to/skills/
#   ├── order-agent/
#   │   └── SKILL.md
#   ├── data-agent/
#   │   └── SKILL.md
#   └── payment-agent/
#       └── SKILL.md

# 容器内自动挂载为:
# /a3s/skills/
#   ├── order-agent/
#   ├── data-agent/
#   └── payment-agent/

# 方式 2: 单独挂载每个技能
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(kind="a3s_code"),
    skills=[
        SkillMount(name="order-agent", path="/path/to/order-agent"),
        SkillMount(name="data-agent", path="/other/path/data-agent"),
    ]
))

# 方式 3: 混合使用（推荐）
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        skills_dir="/path/to/common-skills"  # 通用技能
    ),
    skills=[
        SkillMount(name="custom-agent", path="/path/to/custom-agent"),  # 额外技能
    ]
))
```

### 示例 8: 编码智能体 + 业务智能体

### 示例 8: 自定义资源限制

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(kind="a3s_code"),

    resources=ResourceConfig(
        memory=4 * 1024 * 1024 * 1024,  # 4GB
        cpus=4,
        disk=20 * 1024 * 1024 * 1024,   # 20GB
    )
))
```

### 示例 9: 从 YAML 文件加载配置

```python
import yaml
from a3s_box import create_box, BoxConfig

# 加载配置文件
with open("box-config.yaml") as f:
    config_dict = yaml.safe_load(f)

# 创建配置对象
config = BoxConfig.from_dict(config_dict)

# 创建 Box
box = await create_box(config)
```

**box-config.yaml**:
```yaml
box_id: "my-app-box"

coding_agent:
  kind: "a3s_code"
  version: "0.1.0"
  llm: "./config/llm-config.json"
  skills_dir: "./skills"

skills:
  - name: "custom-agent"
    path: "/path/to/custom-agent"
    auto_activate: true
    env:
      DATABASE_URL: "${DATABASE_URL}"

resources:
  memory: 2147483648  # 2GB
  cpus: 2
  disk: 10737418240   # 10GB

network:
  enable_external: true

workspace: "/workspace"
```

### 示例 10: 环境变量替换

```python
import os
from a3s_box import create_box, BoxConfig, AgentConfig, LLMConfig

box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        llm=LLMConfig(
            provider="anthropic",
            model="claude-3-5-sonnet-20241022",
            api_key=os.getenv("ANTHROPIC_API_KEY"),  # 从环境变量读取
        ),
        env={
            "RUST_LOG": os.getenv("RUST_LOG", "info"),  # 默认值
            "WORKSPACE": os.getenv("WORKSPACE", "/workspace"),
        }
    )
))
```

## TypeScript SDK 示例

### 默认配置

```typescript
import { createBox } from '@a3s/box';

// 使用默认配置
const box = await createBox();

// 使用编码智能体
await box.coding.generate('Write a TypeScript function');
```

### 指定编码智能体

```typescript
import { createBox, BoxConfig, AgentConfig } from '@a3s/box';

// 使用 OpenCode
const box = await createBox({
  codingAgent: {
    kind: 'opencode',
    version: 'latest',
    llm: {
      provider: 'anthropic',
      model: 'claude-3-5-sonnet-20241022',
      apiKey: process.env.ANTHROPIC_API_KEY,
    },
  },
});
```

### 完整配置

```typescript
const box = await createBox({
  boxId: 'my-app-box',

  codingAgent: {
    kind: 'a3s_code',
    version: '0.1.0',
    llm: '/path/to/llm-config.json',  // LLM 配置文件
    skillsDir: '/path/to/skills',     // Skills 目录
  },

  skills: [
    {
      name: 'custom-agent',
      path: '/path/to/custom-agent',
      autoActivate: true,
    },
  ],

  resources: {
    memory: 2 * 1024 * 1024 * 1024,  // 2GB
    cpus: 2,
    disk: 10 * 1024 * 1024 * 1024,   // 10GB
  },

  network: {
    enableExternal: true,
  },

  workspace: '/workspace',
});
```

## 配置验证

A3S Box 会在创建时验证配置：

```python
from a3s_box import create_box, BoxConfig, AgentConfig
from a3s_box.errors import ConfigValidationError

try:
    box = await create_box(BoxConfig(
        coding_agent=AgentConfig(
            kind="oci_image",
            # 错误：缺少 image 参数
        )
    ))
except ConfigValidationError as e:
    print(f"配置错误: {e}")
    # 输出: 配置错误: AgentConfig with kind='oci_image' requires 'image' parameter
```

## 配置最佳实践

### 1. 使用环境变量存储敏感信息

```python
# ✅ 好的做法
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        llm=LLMConfig(
            provider="anthropic",
            api_key=os.getenv("ANTHROPIC_API_KEY"),  # 从环境变量读取
        )
    )
))

# ❌ 不好的做法
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(
        kind="a3s_code",
        llm=LLMConfig(
            provider="anthropic",
            api_key="sk-ant-...",  # 硬编码 API 密钥
        )
    )
))
```

### 2. 使用配置文件管理复杂配置

```python
# ✅ 好的做法：使用 YAML 配置文件
config = BoxConfig.from_yaml("box-config.yaml")
box = await create_box(config)

# ❌ 不好的做法：在代码中硬编码大量配置
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(...),
    skills=[...],
    resources=ResourceConfig(...),
    # ... 很多配置
))
```

### 3. 为不同环境使用不同配置

```python
import os

env = os.getenv("ENV", "development")

if env == "production":
    config = BoxConfig.from_yaml("box-config.prod.yaml")
elif env == "staging":
    config = BoxConfig.from_yaml("box-config.staging.yaml")
else:
    config = BoxConfig.from_yaml("box-config.dev.yaml")

box = await create_box(config)
```

### 4. 验证配置

```python
from a3s_box import BoxConfig

# 加载配置
config = BoxConfig.from_yaml("box-config.yaml")

# 验证配置
errors = config.validate()
if errors:
    for error in errors:
        print(f"配置错误: {error}")
    exit(1)

# 创建 Box
box = await create_box(config)
```

## 配置参考

### 编码智能体类型

| 类型 | 说明 | 必需参数 |
|------|------|----------|
| `a3s_code` | A3S Code（默认） | 无 |
| `opencode` | OpenCode | 无 |
| `oci_image` | OCI 镜像 | `image` |
| `local_binary` | 本地二进制 | `path` |
| `remote_binary` | 远程二进制 | `url`, `checksum` |

### LLM 提供商

| 提供商 | 支持的模型 |
|--------|-----------|
| `anthropic` | claude-3-5-sonnet-20241022, claude-opus-4, claude-3-haiku-20240307 |
| `openai` | gpt-4, gpt-4-turbo, gpt-3.5-turbo |
| `google` | gemini-pro, gemini-ultra |
| `local` | 本地模型（需要配置 base_url） |

### 资源限制

| 参数 | 默认值（编码） | 默认值（业务） | 说明 |
|------|--------------|--------------|------|
| `memory` | 2GB | 1GB | 内存限制 |
| `cpus` | 2 | 1 | CPU 核心数 |
| `disk` | 10GB | 5GB | 磁盘限制 |

## 故障排查

### 问题 1: 编码智能体启动失败

```
Error: Failed to start coding agent: connection timeout
```

**解决方案**:
1. 检查智能体镜像是否存在
2. 检查网络连接
3. 增加启动超时时间

```python
box = await create_box(BoxConfig(
    coding_agent=AgentConfig(kind="a3s_code"),
    startup_timeout=60,  # 增加到 60 秒
))
```

### 问题 2: API 密钥无效

```
Error: Invalid API key for provider 'anthropic'
```

**解决方案**:
1. 检查环境变量是否设置
2. 验证 API 密钥是否有效

```bash
# 检查环境变量
echo $ANTHROPIC_API_KEY

# 设置环境变量
export ANTHROPIC_API_KEY="sk-ant-..."
```

### 问题 3: 资源不足

```
Error: Failed to allocate resources: insufficient memory
```

**解决方案**:
1. 减少资源限制
2. 增加主机资源

```python
box = await create_box(BoxConfig(
    resources=ResourceConfig(
        coding=ContainerResources(
            memory=1 * 1024 * 1024 * 1024,  # 减少到 1GB
            cpus=1,
        )
    )
))
```

---

**版本**: 1.0.0
**最后更新**: 2026-02-03
