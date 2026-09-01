#!/usr/bin/env node
import { run } from "../launcher.mjs";

try {
  await run({ mode: "hook" });
} catch (error) {
  console.error(`ContextStream launcher: ${error?.message ?? error}`);
  process.exitCode = 1;
}
