#!/usr/bin/env node

import { init } from "./init.js";

const args = process.argv.slice(2);
const command = args[0];

if (!command || command === "init") {
  init();
} else if (command === "--help" || command === "-h") {
  console.log(`
recall-echo — Persistent memory for AI coding agents

Usage:
  npx recall-echo init    Initialize recall-echo memory system

Options:
  --help, -h              Show this help message
  --version, -v           Show version

Learn more: https://github.com/dnacenta/recall-echo
`);
} else if (command === "--version" || command === "-v") {
  // Read version from package.json at runtime
  const { readFileSync } = await import("fs");
  const { join, dirname } = await import("path");
  const { fileURLToPath } = await import("url");
  const __dirname = dirname(fileURLToPath(import.meta.url));
  const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf-8"));
  console.log(pkg.version);
} else {
  console.error(`Unknown command: ${command}`);
  console.error(`Run 'recall-echo --help' for usage information.`);
  process.exit(1);
}
