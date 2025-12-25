/**
 * File reading utilities for code indexing
 */

import * as fs from 'fs';
import * as path from 'path';

export interface FileToIngest {
  path: string;
  content: string;
  language?: string;
}

// File extensions to include for indexing
const CODE_EXTENSIONS = new Set([
  // Rust
  'rs',
  // TypeScript/JavaScript
  'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs',
  // Python
  'py', 'pyi',
  // Go
  'go',
  // Java/Kotlin
  'java', 'kt', 'kts',
  // C/C++
  'c', 'h', 'cpp', 'hpp', 'cc', 'cxx',
  // C#
  'cs',
  // Ruby
  'rb',
  // PHP
  'php',
  // Swift
  'swift',
  // Scala
  'scala',
  // Shell
  'sh', 'bash', 'zsh',
  // Config/Data
  'json', 'yaml', 'yml', 'toml', 'xml',
  // SQL
  'sql',
  // Markdown/Docs
  'md', 'markdown', 'rst', 'txt',
  // HTML/CSS
  'html', 'htm', 'css', 'scss', 'sass', 'less',
  // Other
  'graphql', 'proto', 'dockerfile',
]);

// Directories to ignore
const IGNORE_DIRS = new Set([
  'node_modules',
  '.git',
  '.svn',
  '.hg',
  'target',
  'dist',
  'build',
  'out',
  '.next',
  '.nuxt',
  '__pycache__',
  '.pytest_cache',
  '.mypy_cache',
  'venv',
  '.venv',
  'env',
  '.env',
  'vendor',
  'coverage',
  '.coverage',
  '.idea',
  '.vscode',
  '.vs',
]);

// Files to ignore
const IGNORE_FILES = new Set([
  '.DS_Store',
  'Thumbs.db',
  '.gitignore',
  '.gitattributes',
  'package-lock.json',
  'yarn.lock',
  'pnpm-lock.yaml',
  'Cargo.lock',
  'poetry.lock',
  'Gemfile.lock',
  'composer.lock',
]);

// Max file size to index (1MB)
const MAX_FILE_SIZE = 1024 * 1024;

// Max number of files to index in one batch
const MAX_FILES_PER_BATCH = 100;

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

  async function walkDir(dir: string, relativePath: string = ''): Promise<void> {
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
        const ext = entry.name.split('.').pop()?.toLowerCase() ?? '';
        if (!CODE_EXTENSIONS.has(ext)) continue;

        // Check file size
        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;

          // Read file content
          const content = await fs.promises.readFile(fullPath, 'utf-8');
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
 * Read ALL indexable files from a directory (no limit)
 * Returns files in batches via async generator for memory efficiency
 */
export async function* readAllFilesInBatches(
  rootPath: string,
  options: {
    batchSize?: number;
    maxFileSize?: number;
  } = {}
): AsyncGenerator<FileToIngest[], void, unknown> {
  const batchSize = options.batchSize ?? 50;
  const maxFileSize = options.maxFileSize ?? MAX_FILE_SIZE;
  let batch: FileToIngest[] = [];

  async function* walkDir(dir: string, relativePath: string = ''): AsyncGenerator<FileToIngest, void, unknown> {
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

        const ext = entry.name.split('.').pop()?.toLowerCase() ?? '';
        if (!CODE_EXTENSIONS.has(ext)) continue;

        try {
          const stat = await fs.promises.stat(fullPath);
          if (stat.size > maxFileSize) continue;

          const content = await fs.promises.readFile(fullPath, 'utf-8');
          yield { path: relPath, content };
        } catch {
          // Skip unreadable files
        }
      }
    }
  }

  for await (const file of walkDir(rootPath)) {
    batch.push(file);
    if (batch.length >= batchSize) {
      yield batch;
      batch = [];
    }
  }

  if (batch.length > 0) {
    yield batch;
  }
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

        const ext = entry.name.split('.').pop()?.toLowerCase() ?? '';
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
  const ext = filePath.split('.').pop()?.toLowerCase() ?? '';
  const langMap: Record<string, string> = {
    rs: 'rust',
    ts: 'typescript',
    tsx: 'typescript',
    js: 'javascript',
    jsx: 'javascript',
    py: 'python',
    go: 'go',
    java: 'java',
    kt: 'kotlin',
    c: 'c',
    h: 'c',
    cpp: 'cpp',
    hpp: 'cpp',
    cs: 'csharp',
    rb: 'ruby',
    php: 'php',
    swift: 'swift',
    scala: 'scala',
    sql: 'sql',
    md: 'markdown',
    json: 'json',
    yaml: 'yaml',
    yml: 'yaml',
    toml: 'toml',
    html: 'html',
    css: 'css',
    sh: 'shell',
  };
  return langMap[ext] ?? 'unknown';
}
