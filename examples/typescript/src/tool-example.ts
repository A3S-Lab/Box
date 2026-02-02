/**
 * A3S Box Code Agent - Tool Calling Example
 *
 * This example demonstrates the agent using tools to:
 * - Read files
 * - Execute bash commands
 * - Write/edit code
 *
 * Prerequisites:
 * 1. Start the Code Agent: just code serve
 * 2. Set ANTHROPIC_API_KEY or OPENAI_API_KEY environment variable
 *    (or copy .env.example to .env and fill in your API key)
 */

import "./env.js"; // Load .env files
import { A3sClient, getConfig, type StreamChunk } from "@a3s-lab/box";

function streamPromise(
  client: A3sClient,
  sessionId: string,
  prompt: string,
  onChunk: (chunk: StreamChunk) => void
): Promise<void> {
  return new Promise((resolve, reject) => {
    client.streamWithCallback(sessionId, prompt, onChunk, resolve, reject);
  });
}

async function main() {
  console.log("A3S Box Code Agent - Tool Calling Example\n");

  const config = getConfig();
  const client = new A3sClient({
    address: config.serverAddress,
  });

  try {
    // Create session with model configuration
    const sessionId = await client.createSession({
      system:
        "You are a helpful coding assistant with access to file system and bash tools.",
      model: config.model,
    });
    console.log(`Session: ${sessionId}\n`);

    // Example 1: List files
    console.log("=== Example 1: List Files ===\n");
    const listPrompt = "List the files in the current directory";
    console.log(`Prompt: ${listPrompt}\n`);

    await streamPromise(client, sessionId, listPrompt, (chunk) => {
      if (chunk.textDelta) {
        process.stdout.write(chunk.textDelta);
      } else if (chunk.toolCall) {
        console.log(`\n📦 Tool: ${chunk.toolCall.name}`);
        console.log(`   Args: ${chunk.toolCall.args}`);
      } else if (chunk.toolResult) {
        console.log(`\n📋 Result (exit: ${chunk.toolResult.exitCode}):`);
        const output = chunk.toolResult.output;
        const preview =
          output.length > 200 ? output.substring(0, 200) + "..." : output;
        console.log(`   ${preview.replace(/\n/g, "\n   ")}`);
      } else if (chunk.done) {
        console.log("\n");
      }
    });

    // Example 2: Create a file
    console.log("=== Example 2: Create a File ===\n");
    const createPrompt =
      "Create a file called hello.ts with a simple greeting function";
    console.log(`Prompt: ${createPrompt}\n`);

    await streamPromise(client, sessionId, createPrompt, (chunk) => {
      if (chunk.textDelta) {
        process.stdout.write(chunk.textDelta);
      } else if (chunk.toolCall) {
        console.log(`\n📦 Tool: ${chunk.toolCall.name}`);
      } else if (chunk.toolResult) {
        const status =
          chunk.toolResult.exitCode === 0 ? "✅ Success" : "❌ Failed";
        console.log(`\n${status}`);
      } else if (chunk.done) {
        console.log("\n");
      }
    });

    // Example 3: Run a command
    console.log("=== Example 3: Run Command ===\n");
    const runPrompt = "Run the TypeScript file we just created using tsx";
    console.log(`Prompt: ${runPrompt}\n`);

    await streamPromise(client, sessionId, runPrompt, (chunk) => {
      if (chunk.textDelta) {
        process.stdout.write(chunk.textDelta);
      } else if (chunk.toolCall) {
        console.log(`\n📦 Tool: ${chunk.toolCall.name}`);
        console.log(`   Args: ${chunk.toolCall.args}`);
      } else if (chunk.toolResult) {
        console.log(`\n📋 Output:`);
        console.log(`   ${chunk.toolResult.output.replace(/\n/g, "\n   ")}`);
      } else if (chunk.done) {
        console.log("\n");
      }
    });

    // Show steps
    console.log("=== Final Response Summary ===\n");
    const response = await client.generate(
      sessionId,
      "What files did we create and what commands did we run?"
    );
    console.log(response.text);

    // Show all steps taken
    if (response.steps && response.steps.length > 0) {
      console.log("\n=== All Steps ===\n");
      for (const step of response.steps) {
        const preview =
          step.content.length > 100
            ? step.content.substring(0, 100) + "..."
            : step.content;
        console.log(`${step.index + 1}. [${step.stepType}] ${preview}`);
      }
    }

    // Clean up
    await client.destroySession(sessionId);
    console.log("\nSession destroyed.");
  } catch (error) {
    console.error("Error:", error);
    process.exit(1);
  } finally {
    client.close();
  }
}

main();
