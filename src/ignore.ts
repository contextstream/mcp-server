/**
 * .contextstream/ignore support
 *
 * Provides gitignore-style pattern matching for excluding files from indexing.
 * Also used by hooks to determine if a path should be allowed through (not indexed).
 *
 * Usage:
 *   const ig = await loadIgnorePatterns('/path/to/project');
 *   if (ig.ignores('some/path.ts')) {
 *     // File should be excluded from indexing
 *   }
 */

import * as fs from "fs";
import * as path from "path";
import ignore, { Ignore } from "ignore";

const IGNORE_FILENAME = ".contextstream/ignore";

/**
 * Default patterns that are always ignored (in addition to user patterns).
 * These match the existing IGNORE_DIRS and IGNORE_FILES from files.ts.
 */
const DEFAULT_IGNORE_PATTERNS = [
  // Version control
  ".git/",
  ".svn/",
  ".hg/",

  // Package managers / dependencies
  "node_modules/",
  "vendor/",
  ".pnpm/",

  // Build outputs
  "target/",
  "dist/",
  "build/",
  "out/",
  ".next/",
  ".nuxt/",
  ".svelte-kit/",
  ".parcel-cache/",
  ".turbo/",
  ".gradle/",
  ".cache/",
  "bin/",
  "obj/",

  // Python
  "__pycache__/",
  ".pytest_cache/",
  ".mypy_cache/",
  "venv/",
  ".venv/",
  "env/",
  ".env/",

  // IDE
  ".idea/",
  ".vscode/",
  ".vs/",

  // Coverage
  "coverage/",
  ".coverage/",

  // Lock files
  "package-lock.json",
  "yarn.lock",
  "pnpm-lock.yaml",
  "Cargo.lock",
  "poetry.lock",
  "Gemfile.lock",
  "composer.lock",
  "*.min.js",
  "*.min.css",

  // OS files
  ".DS_Store",
  "Thumbs.db",
];

export interface IgnoreInstance {
  /**
   * Check if a path should be ignored (excluded from indexing)
   */
  ignores(pathname: string): boolean;

  /**
   * Get the raw patterns (for debugging/display)
   */
  patterns: string[];

  /**
   * Whether a .contextstream/ignore file was found
   */
  hasUserPatterns: boolean;
}

/**
 * Load ignore patterns from a project directory.
 *
 * Looks for .contextstream/ignore in the project root.
 * Falls back to default patterns if no ignore file exists.
 *
 * @param projectRoot - The root directory of the project
 * @returns An IgnoreInstance with ignores() method
 */
export async function loadIgnorePatterns(projectRoot: string): Promise<IgnoreInstance> {
  const ig = ignore();
  const patterns: string[] = [...DEFAULT_IGNORE_PATTERNS];

  // Add default patterns first
  ig.add(DEFAULT_IGNORE_PATTERNS);

  // Try to load user patterns
  const ignoreFilePath = path.join(projectRoot, IGNORE_FILENAME);
  let hasUserPatterns = false;

  try {
    const content = await fs.promises.readFile(ignoreFilePath, "utf-8");
    const userPatterns = content
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith("#")); // Skip empty lines and comments

    if (userPatterns.length > 0) {
      ig.add(userPatterns);
      patterns.push(...userPatterns);
      hasUserPatterns = true;
    }
  } catch {
    // No ignore file found, use defaults only
  }

  return {
    ignores: (pathname: string) => ig.ignores(pathname),
    patterns,
    hasUserPatterns,
  };
}

/**
 * Synchronous version of loadIgnorePatterns for use in contexts where async isn't available.
 */
export function loadIgnorePatternsSync(projectRoot: string): IgnoreInstance {
  const ig = ignore();
  const patterns: string[] = [...DEFAULT_IGNORE_PATTERNS];

  // Add default patterns first
  ig.add(DEFAULT_IGNORE_PATTERNS);

  // Try to load user patterns
  const ignoreFilePath = path.join(projectRoot, IGNORE_FILENAME);
  let hasUserPatterns = false;

  try {
    const content = fs.readFileSync(ignoreFilePath, "utf-8");
    const userPatterns = content
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith("#"));

    if (userPatterns.length > 0) {
      ig.add(userPatterns);
      patterns.push(...userPatterns);
      hasUserPatterns = true;
    }
  } catch {
    // No ignore file found, use defaults only
  }

  return {
    ignores: (pathname: string) => ig.ignores(pathname),
    patterns,
    hasUserPatterns,
  };
}

/**
 * Check if a path should be ignored using cached patterns.
 *
 * This is a convenience function that loads patterns on first call
 * and caches them for subsequent calls.
 */
const ignoreCache = new Map<string, IgnoreInstance>();

export async function isIgnored(projectRoot: string, pathname: string): Promise<boolean> {
  let ig = ignoreCache.get(projectRoot);
  if (!ig) {
    ig = await loadIgnorePatterns(projectRoot);
    ignoreCache.set(projectRoot, ig);
  }
  return ig.ignores(pathname);
}

/**
 * Clear the ignore cache (useful if .contextstream/ignore changes)
 */
export function clearIgnoreCache(projectRoot?: string): void {
  if (projectRoot) {
    ignoreCache.delete(projectRoot);
  } else {
    ignoreCache.clear();
  }
}

/**
 * Create a sample .contextstream/ignore file with common patterns
 */
export function getSampleIgnoreContent(): string {
  return `# .contextstream/ignore - Additional exclusions from ContextStream indexing
# Uses gitignore syntax: https://git-scm.com/docs/gitignore
#
# Note: Your code is already protected with encryption (TLS 1.3 + AES-256)
# and workspace isolation. This file is for extra-sensitive paths you prefer
# to keep completely off the index.

# Customer/sensitive data
**/customer-data/
**/secrets/
**/*.pem
**/*.key

# Large generated files
**/generated/
**/*.min.js
**/*.min.css

# Test fixtures with sensitive data
**/fixtures/production/
**/test-data/real/

# Vendor code you don't want indexed
**/third-party/
**/external-libs/

# Specific paths in your project
# src/legacy/    # Uncomment to ignore legacy code
# docs/internal/ # Uncomment to ignore internal docs
`;
}
