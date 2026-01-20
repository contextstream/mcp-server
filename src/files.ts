/**
 * File reading utilities for code indexing
 */

import * as fs from "fs";
import * as path from "path";

export interface FileToIngest {
  path: string;
  content: string;
  language?: string;
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

// Max file size to index (1MB)
const MAX_FILE_SIZE = 1024 * 1024;

// Size-based batching configuration (matching API's BatchConfig::for_api())
// Max total bytes per batch (10MB)
const MAX_BATCH_BYTES = 10 * 1024 * 1024;

// Files larger than this are processed individually (2MB)
const LARGE_FILE_THRESHOLD = 2 * 1024 * 1024;

// Soft limit on files per batch (backup limit if size-based batching allows too many)
const MAX_FILES_PER_BATCH = 200;

/**
 * Read all indexable files from a directory
 */
export async function readFilesFromDirectory(
  rootPath: string,
  options: {
    maxFiles?: number;
    maxFileSize?: number;
  } = {}
): Promise<FileToIngest[]> {
  const maxFiles = options.maxFiles ?? MAX_FILES_PER_BATCH;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;
  const files: FileToIngest[] = [];

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
        // Skip ignored directories
        if (IGNORE_DIRS.has(entry.name)) continue;
        await walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        // Skip ignored files
        if (IGNORE_FILES.has(entry.name)) continue;

        // Check extension
        const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
        if (!CODE_EXTENSIONS.has(ext)) continue;

        // Check file size
        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;

          // Read file content
          const content = await fs.promises.readFile(fullPath, "utf-8");
          files.push({
            path: relPath,
            content,
          });
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
    maxFileSize?: number;
  } = {}
): AsyncGenerator<FileToIngest[], void, unknown> {
  const maxBatchBytes = options.maxBatchBytes ?? MAX_BATCH_BYTES;
  const largeFileThreshold = options.largeFileThreshold ?? LARGE_FILE_THRESHOLD;
  const maxFilesPerBatch = options.maxFilesPerBatch ?? MAX_FILES_PER_BATCH;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;

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
        yield* walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        if (IGNORE_FILES.has(entry.name)) continue;

        const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
        if (!CODE_EXTENSIONS.has(ext)) continue;

        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;

          const content = await fs.promises.readFile(fullPath, "utf-8");
          yield { path: relPath, content, sizeBytes: stat.size };
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
      yield [{ path: file.path, content: file.content }];
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
    maxFileSize?: number;
  } = {}
): AsyncGenerator<FileToIngest[], void, unknown> {
  const maxBatchBytes = options.maxBatchBytes ?? MAX_BATCH_BYTES;
  const largeFileThreshold = options.largeFileThreshold ?? LARGE_FILE_THRESHOLD;
  const maxFilesPerBatch = options.maxFilesPerBatch ?? MAX_FILES_PER_BATCH;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;
  const sinceMs = sinceTimestamp.getTime();

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
        yield* walkDir(fullPath, relPath);
      } else if (entry.isFile()) {
        if (IGNORE_FILES.has(entry.name)) continue;

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
          yield { path: relPath, content, sizeBytes: stat.size };
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
      yield [{ path: file.path, content: file.content }];
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
  } = {}
): Promise<{ count: number; stopped: boolean }> {
  const maxFiles = options.maxFiles ?? 1;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;
  let count = 0;
  let stopped = false;

  async function walkDir(dir: string): Promise<void> {
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

      if (entry.isDirectory()) {
        if (IGNORE_DIRS.has(entry.name)) continue;
        await walkDir(fullPath);
      } else if (entry.isFile()) {
        if (IGNORE_FILES.has(entry.name)) continue;

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
