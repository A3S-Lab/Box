/**
 * A3S Box Code Agent Client
 *
 * A TypeScript client for interacting with the A3S Code Agent gRPC service.
 */

import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";
import * as path from "path";
import * as fs from "fs";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// Load proto file - look in multiple locations
function findProtoPath(): string {
  const possiblePaths = [
    // When installed as npm package (proto copied to dist)
    path.resolve(__dirname, "../proto/agent.proto"),
    // When running from SDK source
    path.resolve(__dirname, "../../proto/agent.proto"),
    // When running from monorepo
    path.resolve(__dirname, "../../../code/proto/agent.proto"),
  ];

  for (const p of possiblePaths) {
    try {
      fs.accessSync(p);
      return p;
    } catch {
      continue;
    }
  }

  throw new Error(
    `Could not find agent.proto in any of: ${possiblePaths.join(", ")}`
  );
}

const PROTO_PATH = findProtoPath();

const packageDefinition = protoLoader.loadSync(PROTO_PATH, {
  keepCase: false,
  longs: String,
  enums: String,
  defaults: true,
  oneofs: true,
});

const protoDescriptor = grpc.loadPackageDefinition(packageDefinition) as any;
const AgentService = protoDescriptor.a3s.sandbox.agent.AgentService;

// ============================================================================
// Types
// ============================================================================

export interface TokenUsage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

export interface ToolCall {
  name: string;
  args: string;
}

export interface ToolResult {
  name: string;
  output: string;
  exitCode: number;
}

export interface Step {
  index: number;
  stepType: string;
  content: string;
}

export interface GenerateResponse {
  text: string;
  usage?: TokenUsage;
  toolCalls: ToolCall[];
  toolResults: ToolResult[];
  steps: Step[];
}

export interface StreamChunk {
  textDelta?: string;
  toolCall?: ToolCall;
  toolResult?: ToolResult;
  done?: GenerateResponse;
}

export interface ObjectStreamChunk {
  partialObject?: string;
  done?: { object: any; usage?: TokenUsage };
}

export interface ContextUsage {
  usedTokens: number;
  maxTokens: number;
  percent: number;
  turns: number;
}

export interface Turn {
  role: string;
  content: string;
  timestamp: string;
}

export interface SessionOptions {
  system?: string;
  contextThreshold?: number;
  contextStrategy?: string;
  model?: ModelConfig;
}

export interface ModelConfig {
  provider: string;
  name: string;
  baseUrl?: string;
  apiKey?: string;
}

export interface A3sClientOptions {
  /** Agent server address (default: localhost:4088) */
  address?: string;
  /** gRPC credentials (default: insecure) */
  credentials?: grpc.ChannelCredentials;
}

// ============================================================================
// Client
// ============================================================================

/**
 * A3S Box Code Agent Client
 *
 * Low-level gRPC client for communicating with the A3S Code Agent.
 *
 * @example
 * ```typescript
 * import { A3sClient } from "@a3s-lab/box";
 *
 * const client = new A3sClient({ address: "localhost:4088" });
 * const sessionId = await client.createSession({ system: "You are helpful." });
 * const response = await client.generate(sessionId, "Hello!");
 * console.log(response.text);
 * await client.destroySession(sessionId);
 * client.close();
 * ```
 */
export class A3sClient {
  private client: any;
  private address: string;

  constructor(options: A3sClientOptions = {}) {
    this.address = options.address || "localhost:4088";
    const credentials =
      options.credentials || grpc.credentials.createInsecure();
    this.client = new AgentService(this.address, credentials);
  }

  /**
   * Get the server address
   */
  getAddress(): string {
    return this.address;
  }

  /**
   * Create a new session
   * If model config is provided, it will be configured immediately after creation
   */
  async createSession(options: SessionOptions = {}): Promise<string> {
    const sessionId = await new Promise<string>((resolve, reject) => {
      this.client.createSession(
        {
          system: options.system || "",
          contextThreshold: options.contextThreshold || 0,
          contextStrategy: options.contextStrategy || "",
        },
        (err: grpc.ServiceError | null, response: any) => {
          if (err) reject(err);
          else resolve(response.sessionId);
        }
      );
    });

    // If model config is provided, configure the session
    if (options.model) {
      await this.configure(sessionId, {
        model: options.model,
      });
    }

    return sessionId;
  }

  /**
   * Destroy a session
   */
  destroySession(sessionId: string): Promise<void> {
    return new Promise((resolve, reject) => {
      this.client.destroySession(
        { sessionId },
        (err: grpc.ServiceError | null) => {
          if (err) reject(err);
          else resolve();
        }
      );
    });
  }

  /**
   * Generate a response (non-streaming)
   */
  generate(sessionId: string, prompt: string): Promise<GenerateResponse> {
    return new Promise((resolve, reject) => {
      this.client.generate(
        { sessionId, prompt },
        (err: grpc.ServiceError | null, response: any) => {
          if (err) reject(err);
          else resolve(response);
        }
      );
    });
  }

  /**
   * Generate a response with streaming
   */
  stream(sessionId: string, prompt: string): AsyncIterable<StreamChunk> {
    const call = this.client.stream({ sessionId, prompt });

    return {
      [Symbol.asyncIterator]() {
        return {
          next(): Promise<IteratorResult<StreamChunk>> {
            return new Promise((resolve, reject) => {
              call.once("data", (chunk: any) => {
                resolve({ value: chunk, done: false });
              });
              call.once("end", () => {
                resolve({ value: undefined as any, done: true });
              });
              call.once("error", (err: Error) => {
                reject(err);
              });
            });
          },
        };
      },
    };
  }

  /**
   * Stream with callback (simpler API)
   */
  streamWithCallback(
    sessionId: string,
    prompt: string,
    onChunk: (chunk: StreamChunk) => void,
    onEnd?: () => void,
    onError?: (err: Error) => void
  ): void {
    const call = this.client.stream({ sessionId, prompt });

    call.on("data", (chunk: StreamChunk) => {
      onChunk(chunk);
    });

    call.on("end", () => {
      onEnd?.();
    });

    call.on("error", (err: Error) => {
      onError?.(err);
    });
  }

  /**
   * Generate a structured JSON object
   */
  generateObject(
    sessionId: string,
    prompt: string,
    schema: object
  ): Promise<{ object: any; usage?: TokenUsage }> {
    return new Promise((resolve, reject) => {
      this.client.generateObject(
        { sessionId, prompt, schema: JSON.stringify(schema) },
        (err: grpc.ServiceError | null, response: any) => {
          if (err) reject(err);
          else {
            try {
              resolve({
                object: JSON.parse(response.object),
                usage: response.usage,
              });
            } catch {
              resolve({ object: response.object, usage: response.usage });
            }
          }
        }
      );
    });
  }

  /**
   * Stream a structured JSON object with partial updates
   */
  streamObject(
    sessionId: string,
    prompt: string,
    schema: object
  ): AsyncIterable<ObjectStreamChunk> {
    const call = this.client.streamObject({
      sessionId,
      prompt,
      schema: JSON.stringify(schema),
    });

    return {
      [Symbol.asyncIterator]() {
        return {
          next(): Promise<IteratorResult<ObjectStreamChunk>> {
            return new Promise((resolve, reject) => {
              call.once("data", (chunk: any) => {
                const result: ObjectStreamChunk = {};
                if (chunk.partialObject) {
                  result.partialObject = chunk.partialObject;
                } else if (chunk.done) {
                  try {
                    result.done = {
                      object: JSON.parse(chunk.done.object),
                      usage: chunk.done.usage,
                    };
                  } catch {
                    result.done = {
                      object: chunk.done.object,
                      usage: chunk.done.usage,
                    };
                  }
                }
                resolve({ value: result, done: false });
              });
              call.once("end", () => {
                resolve({ value: undefined as any, done: true });
              });
              call.once("error", (err: Error) => {
                reject(err);
              });
            });
          },
        };
      },
    };
  }

  /**
   * Stream object with callback (simpler API)
   */
  streamObjectWithCallback(
    sessionId: string,
    prompt: string,
    schema: object,
    onChunk: (chunk: ObjectStreamChunk) => void,
    onEnd?: () => void,
    onError?: (err: Error) => void
  ): void {
    const call = this.client.streamObject({
      sessionId,
      prompt,
      schema: JSON.stringify(schema),
    });

    call.on("data", (chunk: any) => {
      const result: ObjectStreamChunk = {};
      if (chunk.partialObject) {
        result.partialObject = chunk.partialObject;
      } else if (chunk.done) {
        try {
          result.done = {
            object: JSON.parse(chunk.done.object),
            usage: chunk.done.usage,
          };
        } catch {
          result.done = {
            object: chunk.done.object,
            usage: chunk.done.usage,
          };
        }
      }
      onChunk(result);
    });

    call.on("end", () => {
      onEnd?.();
    });

    call.on("error", (err: Error) => {
      onError?.(err);
    });
  }

  /**
   * Get context usage for a session
   */
  getContextUsage(sessionId: string): Promise<ContextUsage> {
    return new Promise((resolve, reject) => {
      this.client.getContextUsage(
        { sessionId },
        (err: grpc.ServiceError | null, response: any) => {
          if (err) reject(err);
          else resolve(response);
        }
      );
    });
  }

  /**
   * Get conversation history
   */
  getHistory(sessionId: string): Promise<{ turns: Turn[] }> {
    return new Promise((resolve, reject) => {
      this.client.getHistory(
        { sessionId },
        (err: grpc.ServiceError | null, response: any) => {
          if (err) reject(err);
          else resolve(response);
        }
      );
    });
  }

  /**
   * Clear session history
   */
  clear(sessionId: string): Promise<void> {
    return new Promise((resolve, reject) => {
      this.client.clear({ sessionId }, (err: grpc.ServiceError | null) => {
        if (err) reject(err);
        else resolve();
      });
    });
  }

  /**
   * Compact session context
   */
  compact(sessionId: string): Promise<void> {
    return new Promise((resolve, reject) => {
      this.client.compact({ sessionId }, (err: grpc.ServiceError | null) => {
        if (err) reject(err);
        else resolve();
      });
    });
  }

  /**
   * Configure session settings
   */
  configure(
    sessionId: string,
    options: {
      thinking?: boolean;
      budget?: number;
      model?: ModelConfig;
    }
  ): Promise<void> {
    return new Promise((resolve, reject) => {
      this.client.configure(
        {
          sessionId,
          thinking: options.thinking,
          budget: options.budget,
          model: options.model,
        },
        (err: grpc.ServiceError | null) => {
          if (err) reject(err);
          else resolve();
        }
      );
    });
  }

  /**
   * Health check
   */
  healthCheck(): Promise<boolean> {
    return new Promise((resolve, reject) => {
      this.client.healthCheck(
        {},
        (err: grpc.ServiceError | null, response: any) => {
          if (err) reject(err);
          else resolve(response.healthy);
        }
      );
    });
  }

  /**
   * Cancel ongoing request
   */
  cancel(sessionId: string): Promise<void> {
    return new Promise((resolve, reject) => {
      this.client.cancel({ sessionId }, (err: grpc.ServiceError | null) => {
        if (err) reject(err);
        else resolve();
      });
    });
  }

  /**
   * Close the client connection
   */
  close(): void {
    grpc.closeClient(this.client);
  }
}

export default A3sClient;
