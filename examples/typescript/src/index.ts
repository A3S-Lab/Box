/**
 * A3S Box Code Agent - Basic Usage Example
 *
 * This example demonstrates how to use the A3S Client to:
 * 1. Create a session
 * 2. Send a prompt and get a response
 * 3. View conversation history
 * 4. Clean up the session
 *
 * Environment Variables:
 * - A3S_SERVER_ADDRESS: Agent server address (default: localhost:4088)
 *
 * Prerequisites:
 * 1. Start the Code Agent: just code serve
 * 2. Set ANTHROPIC_API_KEY or OPENAI_API_KEY environment variable
 *    (or copy .env.example to .env and fill in your API key)
 */

import "./env.js"; // Load .env files
import { A3sClient, getConfig, printConfig } from "@a3s-lab/box";

async function main() {
  console.log("A3S Box Code Agent - Basic Example\n");

  // Load and print configuration
  const config = getConfig();
  printConfig(config);

  // Create client
  const client = new A3sClient({
    address: config.serverAddress,
  });

  try {
    // Health check
    console.log("1. Checking agent health...");
    const healthy = await client.healthCheck();
    console.log(`   Agent is ${healthy ? "healthy" : "unhealthy"}\n`);

    if (!healthy) {
      console.error("Agent is not healthy, exiting.");
      process.exit(1);
    }

    // Create a session with model configuration
    console.log("2. Creating session...");
    const sessionId = await client.createSession({
      system: "You are a helpful coding assistant. Be concise and direct.",
      model: config.model,
    });
    console.log(`   Session ID: ${sessionId}\n`);

    // Generate a response
    console.log("3. Sending prompt...");
    const prompt = "Write a hello world function in TypeScript";
    console.log(`   Prompt: "${prompt}"\n`);

    const response = await client.generate(sessionId, prompt);

    console.log("4. Response:");
    console.log("   ---");
    console.log(response.text);
    console.log("   ---\n");

    // Show token usage
    if (response.usage) {
      console.log("5. Token Usage:");
      console.log(`   Prompt tokens: ${response.usage.promptTokens}`);
      console.log(`   Completion tokens: ${response.usage.completionTokens}`);
      console.log(`   Total tokens: ${response.usage.totalTokens}\n`);
    }

    // Show tool calls if any
    if (response.toolCalls && response.toolCalls.length > 0) {
      console.log("6. Tool Calls:");
      for (const tool of response.toolCalls) {
        console.log(`   - ${tool.name}(${tool.args})`);
      }
      console.log();
    }

    // Get context usage
    console.log("7. Context Usage:");
    const contextUsage = await client.getContextUsage(sessionId);
    console.log(`   Used: ${contextUsage.usedTokens} tokens`);
    console.log(`   Max: ${contextUsage.maxTokens} tokens`);
    console.log(`   Percent: ${(contextUsage.percent * 100).toFixed(1)}%`);
    console.log(`   Turns: ${contextUsage.turns}\n`);

    // Get history
    console.log("8. Conversation History:");
    const history = await client.getHistory(sessionId);
    for (const turn of history.turns) {
      const preview =
        turn.content.length > 50
          ? turn.content.substring(0, 50) + "..."
          : turn.content;
      console.log(`   [${turn.role}] ${preview}`);
    }
    console.log();

    // Clean up
    console.log("9. Cleaning up...");
    await client.destroySession(sessionId);
    console.log("   Session destroyed.\n");

    console.log("Done!");
  } catch (error) {
    console.error("Error:", error);
    process.exit(1);
  } finally {
    client.close();
  }
}

main();
