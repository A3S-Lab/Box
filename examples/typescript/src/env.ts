/**
 * Environment Variable Loader
 *
 * Loads environment variables from .env and .env.local files.
 * .env.local takes precedence over .env
 */

import { config } from "dotenv";
import { resolve } from "path";
import { fileURLToPath } from "url";
import { dirname } from "path";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// Load .env file (base configuration)
config({ path: resolve(__dirname, "../.env") });

// Load .env.local file (overrides .env, not committed to git)
config({ path: resolve(__dirname, "../.env.local") });

export function loadEnv(): void {
  // Already loaded via config() calls above
}
