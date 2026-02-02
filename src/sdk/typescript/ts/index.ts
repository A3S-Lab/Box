/**
 * @a3s-lab/box - A3S Box TypeScript SDK
 *
 * This module provides two ways to interact with A3S Box:
 *
 * 1. **A3sClient** - Low-level gRPC client for the Code Agent
 * 2. **Box/createBox** - High-level SDK with VM management (native module)
 *
 * @example
 * ```typescript
 * // Using the gRPC client (requires running Code Agent)
 * import { A3sClient, getConfig } from "@a3s-lab/box";
 *
 * const config = getConfig();
 * const client = new A3sClient({ address: config.serverAddress });
 * const sessionId = await client.createSession({ system: "You are helpful." });
 * const response = await client.generate(sessionId, "Hello!");
 * console.log(response.text);
 * client.close();
 * ```
 *
 * @example
 * ```typescript
 * // Using the high-level SDK (self-contained)
 * import { createBox } from "@a3s-lab/box";
 *
 * const box = await createBox({ workspace: "/tmp/workspace" });
 * const session = box.createSession({ system: "You are helpful." });
 * const result = await session.generate("Hello!");
 * console.log(result.text);
 * await box.destroy();
 * ```
 */

// Re-export gRPC client
export {
  A3sClient,
  type A3sClientOptions,
  type TokenUsage,
  type ToolCall,
  type ToolResult,
  type Step,
  type GenerateResponse,
  type StreamChunk,
  type ObjectStreamChunk,
  type ContextUsage,
  type Turn,
  type SessionOptions,
  type ModelConfig,
} from "./client.js";

// Re-export configuration utilities
export {
  getConfig,
  getModelConfig,
  getDefaultModel,
  printConfig,
  type Config,
} from "./config.js";
