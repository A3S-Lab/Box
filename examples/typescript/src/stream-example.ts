/**
 * A3S Box Code Agent - Streaming Example
 *
 * This example demonstrates streaming responses with real-time output.
 *
 * Prerequisites:
 * 1. Start the Code Agent: just code serve
 * 2. Set ANTHROPIC_API_KEY or OPENAI_API_KEY environment variable
 *    (or copy .env.example to .env and fill in your API key)
 */

import "./env.js"; // Load .env files
import { A3sClient, getConfig } from "@a3s-lab/box";

async function main() {
  console.log("A3S Box Code Agent - Streaming Example\n");

  const config = getConfig();
  const client = new A3sClient({
    address: config.serverAddress,
  });

  try {
    // Create session with model configuration
    const sessionId = await client.createSession({
      system: "You are a helpful coding assistant.",
      model: config.model,
    });
    console.log(`Session: ${sessionId}\n`);

    // Stream a response using callback API (more reliable for gRPC streams)
    const prompt =
      "Explain what a closure is in JavaScript with a simple example";
    console.log(`Prompt: ${prompt}\n`);
    console.log("Response (streaming):");
    console.log("---");

    await new Promise<void>((resolve, reject) => {
      client.streamWithCallback(
        sessionId,
        prompt,
        (chunk) => {
          // Handle different chunk types
          if (chunk.textDelta) {
            process.stdout.write(chunk.textDelta);
          } else if (chunk.toolCall) {
            console.log(`\n[Tool Call: ${chunk.toolCall.name}]`);
          } else if (chunk.toolResult) {
            console.log(
              `\n[Tool Result: ${chunk.toolResult.name} (exit: ${chunk.toolResult.exitCode})]`
            );
          } else if (chunk.done) {
            console.log("\n---");
            console.log("\nGeneration complete!");
            if (chunk.done.usage) {
              console.log(`Total tokens: ${chunk.done.usage.totalTokens}`);
            }
          }
        },
        () => resolve(),
        (err) => reject(err)
      );
    });

    // Multi-turn conversation
    console.log("\n\nContinuing conversation...\n");
    const followUp = "Now show me a practical use case for closures";
    console.log(`Follow-up: ${followUp}\n`);
    console.log("Response:");
    console.log("---");

    await new Promise<void>((resolve, reject) => {
      client.streamWithCallback(
        sessionId,
        followUp,
        (chunk) => {
          if (chunk.textDelta) {
            process.stdout.write(chunk.textDelta);
          } else if (chunk.done) {
            console.log("\n---");
          }
        },
        () => resolve(),
        (err) => reject(err)
      );
    });

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
