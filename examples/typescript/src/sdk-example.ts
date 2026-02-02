/**
 * A3S Box SDK Example
 *
 * This example demonstrates how to use the @a3s-lab/box SDK
 * which provides a higher-level API for creating sandboxed AI agents.
 *
 * Environment Variables:
 * - LLM_PROVIDER: Model provider (anthropic, openai, deepseek, etc.)
 * - LLM_MODEL: Model name
 * - LLM_API_KEY: API key
 * - LLM_BASE_URL: Custom API base URL
 * - A3S_WORKSPACE: Workspace directory
 *
 * Prerequisites:
 * 1. Build the SDK: npm run build:sdk
 * 2. Install dependencies: npm install
 * 3. Copy .env.example to .env and fill in your API key
 */

import "./env.js"; // Load .env files
// Import native module for Box/createBox
import { createBox } from "@a3s-lab/box/native";
// Import TypeScript utilities for config
import { getConfig, printConfig } from "@a3s-lab/box";

async function main() {
  console.log("A3S Box SDK Example\n");

  // Load configuration from environment
  const config = getConfig();
  printConfig(config);

  try {
    // Create a Box instance with environment configuration
    console.log("1. Creating Box...");
    const box = await createBox({
      workspace: config.workspace,
      model: {
        provider: config.model.provider,
        name: config.model.name,
        baseUrl: config.model.baseUrl,
        apiKey: config.model.apiKey,
      },
      resources: {
        vcpus: 2,
        memoryMb: 2048,
        diskMb: 4096,
        timeout: 300,
      },
    });
    console.log("   Box created!\n");

    // Create a session
    console.log("2. Creating session...");
    const session = box.createSession({
      system: "You are a helpful coding assistant.",
    });
    console.log(`   Session ID: ${session.sessionId}\n`);

    // Generate a response
    console.log("3. Generating response...");
    const result = await session.generate("Write a hello world in TypeScript");
    console.log("   Response:");
    console.log(`   ${result.text}\n`);
    console.log("   Usage:");
    console.log(`   - Prompt tokens: ${result.usage.promptTokens}`);
    console.log(`   - Completion tokens: ${result.usage.completionTokens}`);
    console.log(`   - Total tokens: ${result.usage.totalTokens}\n`);

    // Use a skill
    console.log("4. Using skill...");
    await session.useSkill("code-review");
    const skills = await session.listSkills();
    console.log(`   Active skills: ${skills.join(", ") || "none"}\n`);

    // Get context usage
    console.log("5. Context usage...");
    const contextUsage = await session.contextUsage();
    console.log(`   ${contextUsage}\n`);

    // Get metrics
    console.log("6. Box metrics...");
    const metrics = await box.metrics();
    console.log(`   ${metrics}\n`);

    // Clean up
    console.log("7. Cleaning up...");
    await session.destroy();
    await box.destroy();
    console.log("   Done!\n");
  } catch (error) {
    console.error("Error:", error);
    process.exit(1);
  }
}

main();
