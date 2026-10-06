// The one-line descriptions each directory shows, held to the limits those
// directories set: the MCP registry's schema rejects a server.json whose
// description is over 100 characters. Neither the Gemini extensions gallery,
// npm nor Claude Code's plugin list states one, so those are held to a line.
import { readFileSync } from "node:fs";

const limits = [
  ["server.json", (m) => m.description, 100],
  ["gemini-extension.json", (m) => m.description, 250],
  ["npm/mcp/package.json", (m) => m.description, 250],
  [".claude-plugin/plugin.json", (m) => m.description, 250],
  [".claude-plugin/marketplace.json", (m) => m.plugins?.[0]?.description, 250],
];
let failed = false;
for (const [file, read, max] of limits) {
  const description = read(JSON.parse(readFileSync(file, "utf8")));
  // Code points, as JSON Schema's maxLength counts them.
  const length = typeof description === "string" ? [...description].length : 0;
  if (length === 0 || length > max) {
    console.error(`${file}: description is ${length} characters; it must be 1 to ${max}`);
    failed = true;
  }
}
process.exit(failed ? 1 : 0);
