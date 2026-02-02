/**
 * Environment Configuration
 *
 * Reads A3S Box configuration from environment variables.
 *
 * Environment Variables (supports both A3S_* prefixed and non-prefixed):
 * - A3S_LLM_PROVIDER or LLM_PROVIDER: Model provider (anthropic, openai, deepseek, etc.)
 * - A3S_LLM_MODEL or LLM_MODEL: Model name
 * - A3S_LLM_API_KEY or LLM_API_KEY: API key (or use provider-specific: ANTHROPIC_API_KEY, OPENAI_API_KEY)
 * - A3S_LLM_BASE_URL or LLM_BASE_URL: Custom API base URL (for OpenAI-compatible providers)
 * - A3S_WORKSPACE: Workspace directory
 * - A3S_SERVER_ADDRESS: gRPC agent address
 */

export interface ModelConfig {
  provider: string;
  name: string;
  baseUrl?: string;
  apiKey?: string;
}

export interface Config {
  model: ModelConfig;
  workspace: string;
  serverAddress: string;
}

/**
 * Get model configuration from environment variables
 * Supports both A3S_* prefixed and non-prefixed variable names
 * A3S_* prefixed variables take priority
 */
export function getModelConfig(): ModelConfig {
  const provider =
    process.env.A3S_LLM_PROVIDER ||
    process.env.LLM_PROVIDER ||
    "anthropic";
  const name =
    process.env.A3S_LLM_MODEL ||
    process.env.LLM_MODEL ||
    getDefaultModel(provider);
  const apiKey =
    process.env.A3S_LLM_API_KEY ||
    process.env.LLM_API_KEY ||
    process.env.ANTHROPIC_API_KEY ||
    process.env.OPENAI_API_KEY ||
    undefined;
  const baseUrl =
    process.env.A3S_LLM_BASE_URL ||
    process.env.LLM_BASE_URL ||
    undefined;

  return {
    provider,
    name,
    baseUrl,
    apiKey,
  };
}

/**
 * Get default model name for a provider
 */
export function getDefaultModel(provider: string): string {
  switch (provider.toLowerCase()) {
    case "anthropic":
    case "claude":
      return "claude-sonnet-4-20250514";
    case "openai":
    case "gpt":
      return "gpt-4o";
    case "deepseek":
      return "deepseek-chat";
    case "groq":
      return "llama-3.3-70b-versatile";
    case "together":
      return "meta-llama/Llama-3-70b-chat-hf";
    case "ollama":
      return "llama3";
    default:
      return "gpt-4o";
  }
}

/**
 * Get full configuration from environment
 * Supports both A3S_* prefixed and non-prefixed variable names
 * A3S_* prefixed variables take priority
 */
export function getConfig(): Config {
  return {
    model: getModelConfig(),
    workspace: process.env.A3S_WORKSPACE || "/tmp/a3s-workspace",
    serverAddress:
      process.env.A3S_SERVER_ADDRESS || "localhost:4088",
  };
}

/**
 * Print current configuration (for debugging)
 */
export function printConfig(config: Config): void {
  console.log("Configuration:");
  console.log(`  Provider:    ${config.model.provider}`);
  console.log(`  Model:       ${config.model.name}`);
  console.log(`  Base URL:    ${config.model.baseUrl || "(default)"}`);
  console.log(`  API Key:     ${config.model.apiKey ? "***" : "(not set)"}`);
  console.log(`  Workspace:   ${config.workspace}`);
  console.log(`  Server Addr: ${config.serverAddress}`);
  console.log();
}

export default getConfig;
