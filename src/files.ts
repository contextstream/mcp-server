/**
 * File reading utilities for code indexing
 */

import * as fs from "fs";
import * as path from "path";
import { exec } from "child_process";
import { promisify } from "util";
import * as os from "os";
import * as crypto from "crypto";
import { loadIgnorePatterns, IgnoreInstance } from "./ignore.js";

const execAsync = promisify(exec);

export interface FileToIngest {
  path: string;
  content: string;
  language?: string;

  // === Version metadata (for multi-machine sync) ===
  /** Git commit SHA where this file was last modified */
  git_commit_sha?: string;
  /** Timestamp of the git commit (ISO 8601) */
  git_commit_timestamp?: string;
  /** File modification time from the source machine (ISO 8601) */
  source_modified_at?: string;
  /** Stable identifier for the machine doing the indexing */
  machine_id?: string;

  // === Branch metadata ===
  /** Current git branch name (e.g., "feature-auth") */
  git_branch?: string;
  /** Repository's default branch (e.g., "main") */
  git_default_branch?: string;
  /** True if indexing from the default branch */
  is_default_branch?: boolean;
}

// File extensions to include for indexing
const CODE_EXTENSIONS = new Set([
  // Rust
  "rs",
  // TypeScript/JavaScript
  "ts",
  "tsx",
  "js",
  "jsx",
  "mjs",
  "cjs",
  // Python
  "py",
  "pyi",
  // Go
  "go",
  // Java/Kotlin
  "java",
  "kt",
  "kts",
  // C/C++
  "c",
  "h",
  "cpp",
  "hpp",
  "cc",
  "cxx",
  // C#
  "cs",
  // Ruby
  "rb",
  // PHP
  "php",
  // Swift
  "swift",
  // Scala
  "scala",
  // Shell
  "sh",
  "bash",
  "zsh",
  // Config/Data
  "json",
  "yaml",
  "yml",
  "toml",
  "xml",
  // SQL
  "sql",
  // Markdown/Docs
  "md",
  "markdown",
  "rst",
  "txt",
  // HTML/CSS
  "html",
  "htm",
  "css",
  "scss",
  "sass",
  "less",
  // Other
  "graphql",
  "proto",
  "dockerfile",
]);

// Directories to ignore
const IGNORE_DIRS = new Set([
  "node_modules",
  ".git",
  ".svn",
  ".hg",
  "target",
  "dist",
  "build",
  "out",
  ".next",
  ".nuxt",
  "__pycache__",
  ".pytest_cache",
  ".mypy_cache",
  "venv",
  ".venv",
  "env",
  ".env",
  "vendor",
  "coverage",
  ".coverage",
  ".idea",
  ".vscode",
  ".vs",
]);

// Files to ignore
const IGNORE_FILES = new Set([
  ".DS_Store",
  "Thumbs.db",
  ".gitignore",
  ".gitattributes",
  "package-lock.json",
  "yarn.lock",
  "pnpm-lock.yaml",
  "Cargo.lock",
  "poetry.lock",
  "Gemfile.lock",
  "composer.lock",
]);

// Max file size to index (5MB)
const MAX_FILE_SIZE = 5 * 1024 * 1024;

// Size-based batching configuration (matching API's BatchConfig::for_api())
// Max total bytes per batch (10MB)
const MAX_BATCH_BYTES = 10 * 1024 * 1024;

// Files larger than this are processed individually (2MB)
const LARGE_FILE_THRESHOLD = 2 * 1024 * 1024;

// Soft limit on files per batch (backup limit if size-based batching allows too many)
const MAX_FILES_PER_BATCH = 200;

// ============================================================================
// Git Info Extraction (for multi-machine sync)
// ============================================================================

/**
 * Cached git repository info to avoid repeated git commands
 */
interface GitRepoContext {
  isGitRepo: boolean;
  branch?: string;
  defaultBranch?: string;
  isDefaultBranch?: boolean;
  machineId: string;
}

// Cache for git repo context per root path
const gitContextCache = new Map<string, GitRepoContext>();

/**
 * Generate a stable machine identifier from hostname
 * Uses first 12 chars of SHA256 hash for privacy
 */
function getMachineId(): string {
  const hostname = os.hostname();
  const hash = crypto.createHash("sha256").update(hostname).digest("hex");
  return hash.substring(0, 12);
}

/**
 * Check if a directory is inside a git repository
 */
async function isGitRepo(rootPath: string): Promise<boolean> {
  try {
    await execAsync("git rev-parse --is-inside-work-tree", {
      cwd: rootPath,
      timeout: 5000,
    });
    return true;
  } catch {
    return false;
  }
}

/**
 * Get the current git branch name
 */
async function getGitBranch(rootPath: string): Promise<string | undefined> {
  try {
    const { stdout } = await execAsync("git branch --show-current", {
      cwd: rootPath,
      timeout: 5000,
    });
    const branch = stdout.trim();
    return branch || undefined; // Empty string = detached HEAD
  } catch {
    return undefined;
  }
}

/**
 * Get the default branch name (main, master, etc.)
 * Tries multiple methods for compatibility
 */
async function getGitDefaultBranch(
  rootPath: string
): Promise<string | undefined> {
  // Method 1: Check remote HEAD (most reliable when remote is available)
  try {
    const { stdout } = await execAsync(
      "git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null",
      { cwd: rootPath, timeout: 5000 }
    );
    const match = stdout.trim().match(/refs\/remotes\/origin\/(.+)/);
    if (match) return match[1];
  } catch {
    // Fall through to next method
  }

  // Method 2: Check git config for init.defaultBranch
  try {
    const { stdout } = await execAsync(
      "git config --get init.defaultBranch 2>/dev/null",
      { cwd: rootPath, timeout: 5000 }
    );
    const branch = stdout.trim();
    if (branch) return branch;
  } catch {
    // Fall through to next method
  }

  // Method 3: Check if main or master exists
  try {
    const { stdout } = await execAsync("git branch --list main master", {
      cwd: rootPath,
      timeout: 5000,
    });
    const branches = stdout
      .trim()
      .split("\n")
      .map((b) => b.replace(/^\*?\s*/, "").trim())
      .filter(Boolean);
    if (branches.includes("main")) return "main";
    if (branches.includes("master")) return "master";
  } catch {
    // Fall through
  }

  return undefined;
}

/**
 * Get git commit info for a specific file
 * Returns the commit SHA and timestamp where the file was last modified
 */
async function getFileGitInfo(
  rootPath: string,
  relativePath: string
): Promise<{ sha: string; timestamp: string } | undefined> {
  try {
    // Get commit SHA and Unix timestamp in one command
    const { stdout } = await execAsync(
      `git log -1 --format="%H %ct" -- "${relativePath}"`,
      { cwd: rootPath, timeout: 5000 }
    );
    const parts = stdout.trim().split(" ");
    if (parts.length >= 2) {
      const sha = parts[0];
      const unixTimestamp = parseInt(parts[1], 10);
      if (sha && !isNaN(unixTimestamp)) {
        // Convert Unix timestamp to ISO 8601
        const timestamp = new Date(unixTimestamp * 1000).toISOString();
        return { sha, timestamp };
      }
    }
  } catch {
    // File might not be tracked by git
  }
  return undefined;
}

/**
 * Get or create cached git context for a repository
 * This avoids repeating git commands for each file
 */
async function getGitContext(rootPath: string): Promise<GitRepoContext> {
  // Check cache first
  const cached = gitContextCache.get(rootPath);
  if (cached) return cached;

  // Build context
  const machineId = getMachineId();
  const isRepo = await isGitRepo(rootPath);

  if (!isRepo) {
    const context: GitRepoContext = { isGitRepo: false, machineId };
    gitContextCache.set(rootPath, context);
    return context;
  }

  // Get branch info in parallel
  const [branch, defaultBranch] = await Promise.all([
    getGitBranch(rootPath),
    getGitDefaultBranch(rootPath),
  ]);

  const context: GitRepoContext = {
    isGitRepo: true,
    branch,
    defaultBranch,
    isDefaultBranch: branch !== undefined && branch === defaultBranch,
    machineId,
  };

  gitContextCache.set(rootPath, context);
  return context;
}

/**
 * Clear the git context cache (useful for testing or when switching projects)
 */
export function clearGitContextCache(): void {
  gitContextCache.clear();
}

// ============================================================================
// File Reading Functions
// ============================================================================

/**
 * Read all indexable files from a directory
 */
export async function readFilesFromDirectory(
  rootPath: string,
  options: {
    maxFiles?: number;
    maxFileSize?: number;
    ignoreInstance?: IgnoreInstance;
  } = {}
): Promise<FileToIngest[]> {
  const maxFiles = options.maxFiles ?? MAX_FILES_PER_BATCH;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;
  const files: FileToIngest[] = [];

  // Load ignore patterns (uses .contextstream/ignore + defaults)
  const ig = options.ignoreInstance ?? (await loadIgnorePatterns(rootPath));

  // Get git context once (cached) - repo-level info shared across all files
  const gitCtx = await getGitContext(rootPath);

  async function walkDir(dir: string, relativePath: string = ""): Promise<void> {
    if (files.length >= maxFiles) return;

    let entries: fs.Dirent[];
    try {
      entries = await fs.promises.readdir(dir, { withFileTypes: true });
    } catch {
      return; // Skip directories we can't read
    }

    for (const entry of entries) {
      if (files.length >= maxFiles) break;

      const fullPath = path.join(dir, entry.name);
      const relPath = path.join(relativePath, entry.name);

      if (entry.isDirectory()) {
        // Skip ignored directories (check both hardcoded and .contextstream/ignore)
        if (IGNORE_DIRS.has(entry.name)) continue;
        // Check .contextstream/ignore patterns (add trailing slash for directory matching)
        if (ig.ignores(relPath + "/")) continue;
        await walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        // Skip ignored files (check both hardcoded and .contextstream/ignore)
        if (IGNORE_FILES.has(entry.name)) continue;
        if (ig.ignores(relPath)) continue;

        // Check extension
        const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
        if (!CODE_EXTENSIONS.has(ext)) continue;

        // Check file size
        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;

          // Read file content
          const content = await fs.promises.readFile(fullPath, "utf-8");

          // Build file with version metadata
          const file: FileToIngest = {
            path: relPath,
            content,
            // Always include machine_id and source_modified_at
            machine_id: gitCtx.machineId,
            source_modified_at: stat.mtime.toISOString(),
          };

          // Add git info if available
          if (gitCtx.isGitRepo) {
            file.git_branch = gitCtx.branch;
            file.git_default_branch = gitCtx.defaultBranch;
            file.is_default_branch = gitCtx.isDefaultBranch;

            // Get file-specific commit info (silently fails if not tracked)
            const gitInfo = await getFileGitInfo(rootPath, relPath);
            if (gitInfo) {
              file.git_commit_sha = gitInfo.sha;
              file.git_commit_timestamp = gitInfo.timestamp;
            }
          }

          files.push(file);
        } catch {
          // Skip files we can't read
        }
      }
    }
  }

  await walkDir(rootPath);
  return files;
}

/**
 * File with size metadata for size-based batching
 */
interface FileWithSize extends FileToIngest {
  sizeBytes: number;
}

/**
 * Read ALL indexable files from a directory (no limit)
 * Returns files in batches via async generator for memory efficiency
 * Uses SIZE-BASED BATCHING to prevent batch failures from large files
 */
export async function* readAllFilesInBatches(
  rootPath: string,
  options: {
    maxBatchBytes?: number;
    largeFileThreshold?: number;
    maxFilesPerBatch?: number;
    batchSize?: number; // Alias for maxFilesPerBatch
    maxFileSize?: number;
    ignoreInstance?: IgnoreInstance;
  } = {}
): AsyncGenerator<FileToIngest[], void, unknown> {
  const maxBatchBytes = options.maxBatchBytes ?? MAX_BATCH_BYTES;
  const largeFileThreshold = options.largeFileThreshold ?? LARGE_FILE_THRESHOLD;
  const maxFilesPerBatch = options.maxFilesPerBatch ?? options.batchSize ?? MAX_FILES_PER_BATCH;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;

  // Load ignore patterns (uses .contextstream/ignore + defaults)
  const ig = options.ignoreInstance ?? (await loadIgnorePatterns(rootPath));

  // Get git context once (cached) - repo-level info shared across all files
  const gitCtx = await getGitContext(rootPath);

  let batch: FileWithSize[] = [];
  let currentBatchBytes = 0;

  async function* walkDir(
    dir: string,
    relativePath: string = ""
  ): AsyncGenerator<FileWithSize, void, unknown> {
    let entries: fs.Dirent[];
    try {
      entries = await fs.promises.readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }

    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      const relPath = path.join(relativePath, entry.name);

      if (entry.isDirectory()) {
        if (IGNORE_DIRS.has(entry.name)) continue;
        // Check .contextstream/ignore patterns
        if (ig.ignores(relPath + "/")) continue;
        yield* walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        if (IGNORE_FILES.has(entry.name)) continue;
        if (ig.ignores(relPath)) continue;

        const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
        if (!CODE_EXTENSIONS.has(ext)) continue;

        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;

          const content = await fs.promises.readFile(fullPath, "utf-8");

          // Build file with version metadata
          const file: FileWithSize = {
            path: relPath,
            content,
            sizeBytes: stat.size,
            // Always include machine_id and source_modified_at
            machine_id: gitCtx.machineId,
            source_modified_at: stat.mtime.toISOString(),
          };

          // Add git info if available
          if (gitCtx.isGitRepo) {
            file.git_branch = gitCtx.branch;
            file.git_default_branch = gitCtx.defaultBranch;
            file.is_default_branch = gitCtx.isDefaultBranch;

            // Get file-specific commit info (silently fails if not tracked)
            const gitInfo = await getFileGitInfo(rootPath, relPath);
            if (gitInfo) {
              file.git_commit_sha = gitInfo.sha;
              file.git_commit_timestamp = gitInfo.timestamp;
            }
          }

          yield file;
        } catch {
          // Skip unreadable files
        }
      }
    }
  }

  for await (const file of walkDir(rootPath)) {
    // Large files are processed individually to prevent batch failures
    if (file.sizeBytes > largeFileThreshold) {
      // First, yield current batch if not empty
      if (batch.length > 0) {
        yield batch.map(({ sizeBytes, ...rest }) => rest);
        batch = [];
        currentBatchBytes = 0;
      }
      // Yield large file as its own batch
      const { sizeBytes, ...fileData } = file;
      yield [fileData];
      continue;
    }

    // Check if adding this file would exceed limits
    const wouldExceedBytes = currentBatchBytes + file.sizeBytes > maxBatchBytes;
    const wouldExceedFiles = batch.length >= maxFilesPerBatch;

    if (wouldExceedBytes || wouldExceedFiles) {
      if (batch.length > 0) {
        yield batch.map(({ sizeBytes, ...rest }) => rest);
        batch = [];
        currentBatchBytes = 0;
      }
    }

    // Add to current batch
    batch.push(file);
    currentBatchBytes += file.sizeBytes;
  }

  if (batch.length > 0) {
    yield batch.map(({ sizeBytes, ...rest }) => rest);
  }
}

/**
 * Read only files that have been modified since a given timestamp.
 * Used for incremental indexing to avoid re-processing unchanged files.
 * Returns files in batches via async generator for memory efficiency.
 * Uses SIZE-BASED BATCHING to prevent batch failures from large files.
 */
export async function* readChangedFilesInBatches(
  rootPath: string,
  sinceTimestamp: Date,
  options: {
    maxBatchBytes?: number;
    largeFileThreshold?: number;
    maxFilesPerBatch?: number;
    batchSize?: number; // Alias for maxFilesPerBatch
    maxFileSize?: number;
    ignoreInstance?: IgnoreInstance;
  } = {}
): AsyncGenerator<FileToIngest[], void, unknown> {
  const maxBatchBytes = options.maxBatchBytes ?? MAX_BATCH_BYTES;
  const largeFileThreshold = options.largeFileThreshold ?? LARGE_FILE_THRESHOLD;
  const maxFilesPerBatch = options.maxFilesPerBatch ?? options.batchSize ?? MAX_FILES_PER_BATCH;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;
  const sinceMs = sinceTimestamp.getTime();

  // Load ignore patterns (uses .contextstream/ignore + defaults)
  const ig = options.ignoreInstance ?? (await loadIgnorePatterns(rootPath));

  // Get git context once (cached) - repo-level info shared across all files
  const gitCtx = await getGitContext(rootPath);

  let batch: FileWithSize[] = [];
  let currentBatchBytes = 0;
  let filesScanned = 0;
  let filesChanged = 0;

  async function* walkDir(
    dir: string,
    relativePath: string = ""
  ): AsyncGenerator<FileWithSize, void, unknown> {
    let entries: fs.Dirent[];
    try {
      entries = await fs.promises.readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }

    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      const relPath = path.join(relativePath, entry.name);

      if (entry.isDirectory()) {
        if (IGNORE_DIRS.has(entry.name)) continue;
        // Check .contextstream/ignore patterns
        if (ig.ignores(relPath + "/")) continue;
        yield* walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        if (IGNORE_FILES.has(entry.name)) continue;
        if (ig.ignores(relPath)) continue;

        const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
        if (!CODE_EXTENSIONS.has(ext)) continue;

        try {
          const stat = await fs.promises.stat(fullPath);
          filesScanned++;

          // Skip files not modified since the last index
          if (stat.mtimeMs <= sinceMs) continue;

          if (stat.size > maxFileSize) continue;

          const content = await fs.promises.readFile(fullPath, "utf-8");
          filesChanged++;

          // Build file with version metadata
          const file: FileWithSize = {
            path: relPath,
            content,
            sizeBytes: stat.size,
            // Always include machine_id and source_modified_at
            machine_id: gitCtx.machineId,
            source_modified_at: stat.mtime.toISOString(),
          };

          // Add git info if available
          if (gitCtx.isGitRepo) {
            file.git_branch = gitCtx.branch;
            file.git_default_branch = gitCtx.defaultBranch;
            file.is_default_branch = gitCtx.isDefaultBranch;

            // Get file-specific commit info (silently fails if not tracked)
            const gitInfo = await getFileGitInfo(rootPath, relPath);
            if (gitInfo) {
              file.git_commit_sha = gitInfo.sha;
              file.git_commit_timestamp = gitInfo.timestamp;
            }
          }

          yield file;
        } catch {
          // Skip unreadable files
        }
      }
    }
  }

  for await (const file of walkDir(rootPath)) {
    // Large files are processed individually to prevent batch failures
    if (file.sizeBytes > largeFileThreshold) {
      // First, yield current batch if not empty
      if (batch.length > 0) {
        yield batch.map(({ sizeBytes, ...rest }) => rest);
        batch = [];
        currentBatchBytes = 0;
      }
      // Yield large file as its own batch
      const { sizeBytes, ...fileData } = file;
      yield [fileData];
      continue;
    }

    // Check if adding this file would exceed limits
    const wouldExceedBytes = currentBatchBytes + file.sizeBytes > maxBatchBytes;
    const wouldExceedFiles = batch.length >= maxFilesPerBatch;

    if (wouldExceedBytes || wouldExceedFiles) {
      if (batch.length > 0) {
        yield batch.map(({ sizeBytes, ...rest }) => rest);
        batch = [];
        currentBatchBytes = 0;
      }
    }

    // Add to current batch
    batch.push(file);
    currentBatchBytes += file.sizeBytes;
  }

  if (batch.length > 0) {
    yield batch.map(({ sizeBytes, ...rest }) => rest);
  }

  console.error(
    `[ContextStream] Incremental scan: ${filesChanged} changed files out of ${filesScanned} scanned (since ${sinceTimestamp.toISOString()})`
  );
}

/**
 * Check if a directory contains any indexable files.
 * Stops as soon as it finds one file for efficiency.
 * Returns count=0 if directory is empty or has no indexable files.
 */
export async function countIndexableFiles(
  rootPath: string,
  options: {
    maxFiles?: number; // Stop counting after this many (default 1 for quick check)
    maxFileSize?: number;
    ignoreInstance?: IgnoreInstance;
  } = {}
): Promise<{ count: number; stopped: boolean }> {
  const maxFiles = options.maxFiles ?? 1;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;

  // Load ignore patterns (uses .contextstream/ignore + defaults)
  const ig = options.ignoreInstance ?? (await loadIgnorePatterns(rootPath));

  let count = 0;
  let stopped = false;

  async function walkDir(dir: string, relativePath: string = ""): Promise<void> {
    if (count >= maxFiles) {
      stopped = true;
      return;
    }

    let entries: fs.Dirent[];
    try {
      entries = await fs.promises.readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }

    for (const entry of entries) {
      if (count >= maxFiles) {
        stopped = true;
        return;
      }

      const fullPath = path.join(dir, entry.name);
      const relPath = path.join(relativePath, entry.name);

      if (entry.isDirectory()) {
        if (IGNORE_DIRS.has(entry.name)) continue;
        // Check .contextstream/ignore patterns
        if (ig.ignores(relPath + "/")) continue;
        await walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        if (IGNORE_FILES.has(entry.name)) continue;
        if (ig.ignores(relPath)) continue;

        const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
        if (!CODE_EXTENSIONS.has(ext)) continue;

        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;
          count++;
          if (count >= maxFiles) {
            stopped = true;
            return;
          }
        } catch {
          // Skip unreadable files
        }
      }
    }
  }

  await walkDir(rootPath);
  return { count, stopped };
}

/**
 * Get language from file extension
 */
export function detectLanguage(filePath: string): string {
  const ext = filePath.split(".").pop()?.toLowerCase() ?? "";
  const langMap: Record<string, string> = {
    rs: "rust",
    ts: "typescript",
    tsx: "typescript",
    js: "javascript",
    jsx: "javascript",
    py: "python",
    go: "go",
    java: "java",
    kt: "kotlin",
    c: "c",
    h: "c",
    cpp: "cpp",
    hpp: "cpp",
    cs: "csharp",
    rb: "ruby",
    php: "php",
    swift: "swift",
    scala: "scala",
    sql: "sql",
    md: "markdown",
    json: "json",
    yaml: "yaml",
    yml: "yaml",
    toml: "toml",
    html: "html",
    css: "css",
    sh: "shell",
  };
  return langMap[ext] ?? "unknown";
}

// ============================================================================
// Content Hash Manifest — SHA-256 per file to skip unchanged files on re-index
// ============================================================================

/**
 * Compute SHA-256 hex digest of a string.
 */
export function sha256Hex(content: string): string {
  return crypto.createHash("sha256").update(content).digest("hex");
}

/**
 * Get path to ~/.contextstream/file-hashes/{projectId}.json
 */
function hashManifestPath(projectId: string): string {
  return path.join(os.homedir(), ".contextstream", "file-hashes", `${projectId}.json`);
}

/**
 * Load the hash manifest for a project.
 * Returns a Map of relative path → sha256 hex.
 * Best-effort: returns empty map on any error.
 */
export function readHashManifest(projectId: string): Map<string, string> {
  try {
    const content = fs.readFileSync(hashManifestPath(projectId), "utf-8");
    const parsed = JSON.parse(content) as Record<string, string>;
    return new Map(Object.entries(parsed));
  } catch {
    return new Map();
  }
}

/**
 * Write the hash manifest for a project. Best-effort.
 */
export function writeHashManifest(projectId: string, hashes: Map<string, string>): void {
  const filePath = hashManifestPath(projectId);
  try {
    const dir = path.dirname(filePath);
    fs.mkdirSync(dir, { recursive: true });
    const obj = Object.fromEntries(hashes);
    fs.writeFileSync(filePath, JSON.stringify(obj, null, 2));
  } catch {
    // Best-effort: silently ignore write failures
  }
}

/**
 * Delete the hash manifest for a project. Best-effort.
 */
export function deleteHashManifest(projectId: string): void {
  try {
    fs.unlinkSync(hashManifestPath(projectId));
  } catch {
    // Ignore errors (file may not exist)
  }
}
