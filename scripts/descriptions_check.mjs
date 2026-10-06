// The one-line descriptions each directory shows, held to the limits those
// directories set: the MCP registry's schema rejects a server.json whose
// description is over 100 characters. Neither the Gemini extensions gallery
// nor npm states one, so those are held to a line a card can show whole.
import { readFileSync } from "node:fs";

const limits = [
  ["server.json", 100],
  ["gemini-extension.json", 220],
  ["npm/mcp/package.json", 220],
  [".claude-plugin/plugin.json", 220],
];
let failed = false;
for (const [file, max] of limits) {
  const { description } = JSON.parse(readFileSync(file, "utf8"));
  const length = [...(description ?? "")].length;
  if (length === 0 || length > max) {
    console.error(`${file}: description is ${length} characters; it must be 1 to ${max}`);
    failed = true;
  }
}
process.exit(failed ? 1 : 0);
