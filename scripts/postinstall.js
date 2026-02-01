#!/usr/bin/env node
import { existsSync, readFileSync, writeFileSync, mkdirSync } from "fs";
import { homedir } from "os";
import { join, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkgPath = join(__dirname, "..", "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));
const binPath = join(__dirname, "..", "dist", "index.js");
const claudeDir = join(homedir(), ".claude");
const claudeSettingsPath = join(claudeDir, "settings.json");
const versionFile = join(claudeDir, ".contextstream-version");

// Ensure .claude directory exists
if (!existsSync(claudeDir)) {
  mkdirSync(claudeDir, { recursive: true });
}

// Store version for update detection
writeFileSync(versionFile, pkg.version);

// Auto-configure hooks if settings.json exists
if (existsSync(claudeSettingsPath)) {
  try {
    let content = readFileSync(claudeSettingsPath, "utf8");
    const originalContent = content;

    // Replace npx commands with direct node execution
    content = content.replace(
      /npx\s+@contextstream\/mcp-server/g,
      `node ${binPath}`
    );

    // Also handle any existing direct paths that might be outdated
    content = content.replace(
      /node\s+[^\s"]+mcp-server\/dist\/index\.js/g,
      `node ${binPath}`
    );

    if (content !== originalContent) {
      writeFileSync(claudeSettingsPath, content);
      console.log("✓ ContextStream hooks updated for optimal performance");
      console.log(`  Using: ${binPath}`);
    } else {
      console.log("✓ ContextStream hooks already configured");
    }
  } catch (err) {
    console.warn("⚠ Could not auto-configure hooks:", err.message);
    console.log("  Run: contextstream-mcp init");
  }
} else {
  console.log("ℹ Claude settings not found. Run: contextstream-mcp init");
}

console.log(`✓ ContextStream MCP Server v${pkg.version} installed`);
