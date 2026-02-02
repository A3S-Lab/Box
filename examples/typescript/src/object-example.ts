/**
 * A3S Box Code Agent - Structured Object Generation Example
 *
 * This example demonstrates generateObject and streamObject methods
 * for generating structured JSON objects with schema validation.
 *
 * Prerequisites:
 * 1. Start the Code Agent: just code serve
 * 2. Set ANTHROPIC_API_KEY or OPENAI_API_KEY environment variable
 *    (or copy .env.example to .env and fill in your API key)
 */

import "./env.js"; // Load .env files
import { A3sClient, getConfig } from "@a3s-lab/box";

// Define JSON schemas for structured output
const personSchema = {
  type: "object",
  properties: {
    name: { type: "string", description: "Full name of the person" },
    age: { type: "number", description: "Age in years" },
    email: { type: "string", description: "Email address" },
    skills: {
      type: "array",
      items: { type: "string" },
      description: "List of skills",
    },
  },
  required: ["name", "age", "email", "skills"],
};

const taskListSchema = {
  type: "object",
  properties: {
    project: { type: "string", description: "Project name" },
    tasks: {
      type: "array",
      items: {
        type: "object",
        properties: {
          id: { type: "number" },
          title: { type: "string" },
          priority: { type: "string", enum: ["low", "medium", "high"] },
          completed: { type: "boolean" },
        },
        required: ["id", "title", "priority", "completed"],
      },
    },
  },
  required: ["project", "tasks"],
};

async function main() {
  console.log("A3S Box Code Agent - Structured Object Generation Example\n");

  const config = getConfig();
  const client = new A3sClient({
    address: config.serverAddress,
  });

  try {
    // Create session with model configuration
    const sessionId = await client.createSession({
      system: "You are a helpful assistant that generates structured data.",
      model: config.model,
    });
    console.log(`Session: ${sessionId}\n`);

    // =========================================================================
    // Example 1: generateObject - Non-streaming structured output
    // =========================================================================
    console.log("=".repeat(60));
    console.log("Example 1: generateObject (non-streaming)");
    console.log("=".repeat(60));

    const prompt1 =
      "Generate a fictional software developer profile with name, age, email, and 5 programming skills.";
    console.log(`\nPrompt: ${prompt1}\n`);

    const result1 = await client.generateObject(sessionId, prompt1, personSchema);

    console.log("Generated Object:");
    console.log(JSON.stringify(result1.object, null, 2));

    if (result1.usage) {
      console.log(`\nToken Usage: ${result1.usage.totalTokens} total`);
    }

    // =========================================================================
    // Example 2: generateObject - More complex schema
    // =========================================================================
    console.log("\n" + "=".repeat(60));
    console.log("Example 2: generateObject with complex schema");
    console.log("=".repeat(60));

    const prompt2 =
      "Generate a task list for a web application project with 4 tasks of varying priorities.";
    console.log(`\nPrompt: ${prompt2}\n`);

    const result2 = await client.generateObject(sessionId, prompt2, taskListSchema);

    console.log("Generated Object:");
    console.log(JSON.stringify(result2.object, null, 2));

    if (result2.usage) {
      console.log(`\nToken Usage: ${result2.usage.totalTokens} total`);
    }

    // =========================================================================
    // Example 3: streamObject - Streaming structured output
    // =========================================================================
    console.log("\n" + "=".repeat(60));
    console.log("Example 3: streamObject (streaming)");
    console.log("=".repeat(60));

    const prompt3 =
      "Generate another developer profile with different skills focused on data science.";
    console.log(`\nPrompt: ${prompt3}\n`);
    console.log("Streaming partial objects:");
    console.log("---");

    await new Promise<void>((resolve, reject) => {
      client.streamObjectWithCallback(
        sessionId,
        prompt3,
        personSchema,
        (chunk) => {
          if (chunk.partialObject) {
            // Show partial JSON as it streams
            process.stdout.write(`\rPartial: ${chunk.partialObject.slice(0, 80)}...`);
          } else if (chunk.done) {
            console.log("\n---");
            console.log("\nFinal Object:");
            console.log(JSON.stringify(chunk.done.object, null, 2));
            if (chunk.done.usage) {
              console.log(`\nToken Usage: ${chunk.done.usage.totalTokens} total`);
            }
          }
        },
        () => resolve(),
        (err) => reject(err)
      );
    });

    // =========================================================================
    // Example 4: streamObject with AsyncIterator
    // =========================================================================
    console.log("\n" + "=".repeat(60));
    console.log("Example 4: streamObject with AsyncIterator");
    console.log("=".repeat(60));

    const prompt4 =
      "Generate a task list for a mobile app project with 3 high-priority tasks.";
    console.log(`\nPrompt: ${prompt4}\n`);
    console.log("Streaming with async iterator:");
    console.log("---");

    let chunkCount = 0;
    for await (const chunk of client.streamObject(sessionId, prompt4, taskListSchema)) {
      if (chunk.partialObject) {
        chunkCount++;
        if (chunkCount % 5 === 0) {
          // Print every 5th chunk to avoid too much output
          console.log(`Chunk ${chunkCount}: ${chunk.partialObject.slice(0, 60)}...`);
        }
      } else if (chunk.done) {
        console.log("---");
        console.log(`\nReceived ${chunkCount} partial chunks`);
        console.log("\nFinal Object:");
        console.log(JSON.stringify(chunk.done.object, null, 2));
        if (chunk.done.usage) {
          console.log(`\nToken Usage: ${chunk.done.usage.totalTokens} total`);
        }
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
