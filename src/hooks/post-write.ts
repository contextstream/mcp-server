/**
 * ContextStream PostToolUse Hook - Real-time file indexing
 *
 * Called after Edit/Write/NotebookEdit tools complete to index the changed file.
 * Supports multiple editors: Claude Code, Cursor, Windsurf, Cline, Roo, Kilo.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook post-write
 *
 * Or directly:
 *   node dist/hooks/post-write.js
 *
 * Input (stdin): JSON with tool_input containing file_path
 * Output: None (fire and forget)
 * Exit: Always 0 (non-blocking)
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

// Environment variables (inherited from MCP server process)
const API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
const API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
const ENABLED = process.env.CONTEXTSTREAM_POSTWRITE_ENABLED !== "false";

// File extensions to index (skip binaries, large files, etc.)
const INDEXABLE_EXTENSIONS = new Set([
  ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs",
  ".py", ".pyw",
  ".rs", ".go", ".java", ".kt", ".scala",
  ".c", ".cpp", ".cc", ".cxx", ".h", ".hpp",
  ".cs", ".fs", ".vb",
  ".rb", ".php", ".pl", ".pm",
  ".swift", ".m", ".mm",
  ".lua", ".r", ".jl",
  ".sh", ".bash", ".zsh", ".fish",
  ".sql", ".graphql", ".gql",
  ".html", ".htm", ".css", ".scss", ".sass", ".less",
  ".json", ".yaml", ".yml", ".toml", ".xml", ".ini", ".cfg",
  ".md", ".mdx", ".txt", ".rst",
  ".vue", ".svelte", ".astro",
  ".tf", ".hcl",
  ".dockerfile", ".containerfile",
  ".prisma", ".proto",
]);

// Max file size to index (5MB)
const MAX_FILE_SIZE = 5 * 1024 * 1024;

interface HookInput {
  // Claude Code format
  tool_name?: string;
  tool_input?: {
    file_path?: string;
    notebook_path?: string;
    path?: string;
  };
  cwd?: string;

  // Cursor format
  hook_event_name?: string;
  parameters?: {
    path?: string;
    file_path?: string;
  };
  workspace_roots?: string[];

  // Cline/Roo/Kilo format
  hookName?: string;
  toolName?: string;
  toolParameters?: {
    path?: string;
    content?: string;
  };
  workspaceRoots?: string[];

  // Windsurf format
  file_path?: string;
  file_content?: string;
}

interface LocalConfig {
  workspace_id?: string;
  project_id?: string;
  project_name?: string;
}

interface McpConfig {
  mcpServers?: {
    contextstream?: {
      env?: {
        CONTEXTSTREAM_API_KEY?: string;
        CONTEXTSTREAM_API_URL?: string;
      };
    };
  };
}

/**
 * Extract file path from hook input, handling different editor formats.
 */
function extractFilePath(input: HookInput): string | null {
  // Claude Code: tool_input.file_path or tool_input.notebook_path
  if (input.tool_input) {
    const filePath = input.tool_input.file_path || input.tool_input.notebook_path || input.tool_input.path;
    if (filePath) return filePath;
  }

  // Cursor: parameters.path or parameters.file_path
  if (input.parameters) {
    const filePath = input.parameters.path || input.parameters.file_path;
    if (filePath) return filePath;
  }

  // Cline/Roo/Kilo: toolParameters.path
  if (input.toolParameters?.path) {
    return input.toolParameters.path;
  }

  // Windsurf: direct file_path
  if (input.file_path) {
    return input.file_path;
  }

  return null;
}

/**
 * Extract working directory from hook input.
 */
function extractCwd(input: HookInput): string {
  // Claude Code
  if (input.cwd) return input.cwd;

  // Cursor
  if (input.workspace_roots?.length) return input.workspace_roots[0];

  // Cline/Roo/Kilo
  if (input.workspaceRoots?.length) return input.workspaceRoots[0];

  return process.cwd();
}

/**
 * Find .contextstream/config.json by traversing up from a directory.
 */
function findLocalConfig(startDir: string): LocalConfig | null {
  let currentDir = path.resolve(startDir);

  for (let i = 0; i < 10; i++) {
    const configPath = path.join(currentDir, ".contextstream", "config.json");
    if (fs.existsSync(configPath)) {
      try {
        const content = fs.readFileSync(configPath, "utf-8");
        return JSON.parse(content) as LocalConfig;
      } catch {
        // Continue searching
      }
    }

    const parentDir = path.dirname(currentDir);
    if (parentDir === currentDir) break;
    currentDir = parentDir;
  }

  return null;
}

/**
 * Load API config from .mcp.json if env vars not set.
 */
function loadApiConfig(startDir: string): { apiUrl: string; apiKey: string } {
  let apiUrl = API_URL;
  let apiKey = API_KEY;

  if (apiKey) {
    return { apiUrl, apiKey };
  }

  // Search for .mcp.json
  let currentDir = path.resolve(startDir);
  for (let i = 0; i < 10; i++) {
    const mcpPath = path.join(currentDir, ".mcp.json");
    if (fs.existsSync(mcpPath)) {
      try {
        const content = fs.readFileSync(mcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const csEnv = config.mcpServers?.contextstream?.env;
        if (csEnv?.CONTEXTSTREAM_API_KEY) {
          apiKey = csEnv.CONTEXTSTREAM_API_KEY;
        }
        if (csEnv?.CONTEXTSTREAM_API_URL) {
          apiUrl = csEnv.CONTEXTSTREAM_API_URL;
        }
        if (apiKey) break;
      } catch {
        // Continue searching
      }
    }

    const parentDir = path.dirname(currentDir);
    if (parentDir === currentDir) break;
    currentDir = parentDir;
  }

  // Also check home directory
  if (!apiKey) {
    const homeMcpPath = path.join(homedir(), ".mcp.json");
    if (fs.existsSync(homeMcpPath)) {
      try {
        const content = fs.readFileSync(homeMcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const csEnv = config.mcpServers?.contextstream?.env;
        if (csEnv?.CONTEXTSTREAM_API_KEY) {
          apiKey = csEnv.CONTEXTSTREAM_API_KEY;
        }
        if (csEnv?.CONTEXTSTREAM_API_URL) {
          apiUrl = csEnv.CONTEXTSTREAM_API_URL;
        }
      } catch {
        // Ignore
      }
    }
  }

  return { apiUrl, apiKey };
}

/**
 * Check if a file should be indexed based on extension and size.
 */
function shouldIndexFile(filePath: string): boolean {
  const ext = path.extname(filePath).toLowerCase();

  // Check extension
  if (!INDEXABLE_EXTENSIONS.has(ext)) {
    // Allow some special files without extensions
    const basename = path.basename(filePath).toLowerCase();
    if (!["dockerfile", "makefile", "rakefile", "gemfile", "procfile"].includes(basename)) {
      return false;
    }
  }

  // Check file size
  try {
    const stats = fs.statSync(filePath);
    if (stats.size > MAX_FILE_SIZE) {
      return false;
    }
  } catch {
    return false;
  }

  return true;
}

/**
 * Detect language from file path.
 */
function detectLanguage(filePath: string): string {
  const ext = path.extname(filePath).toLowerCase();
  const langMap: Record<string, string> = {
    ".ts": "typescript",
    ".tsx": "typescript",
    ".js": "javascript",
    ".jsx": "javascript",
    ".mjs": "javascript",
    ".cjs": "javascript",
    ".py": "python",
    ".pyw": "python",
    ".rs": "rust",
    ".go": "go",
    ".java": "java",
    ".kt": "kotlin",
    ".scala": "scala",
    ".c": "c",
    ".cpp": "cpp",
    ".cc": "cpp",
    ".cxx": "cpp",
    ".h": "c",
    ".hpp": "cpp",
    ".cs": "csharp",
    ".fs": "fsharp",
    ".vb": "vb",
    ".rb": "ruby",
    ".php": "php",
    ".pl": "perl",
    ".pm": "perl",
    ".swift": "swift",
    ".m": "objective-c",
    ".mm": "objective-cpp",
    ".lua": "lua",
    ".r": "r",
    ".jl": "julia",
    ".sh": "shell",
    ".bash": "shell",
    ".zsh": "shell",
    ".fish": "shell",
    ".sql": "sql",
    ".graphql": "graphql",
    ".gql": "graphql",
    ".html": "html",
    ".htm": "html",
    ".css": "css",
    ".scss": "scss",
    ".sass": "sass",
    ".less": "less",
    ".json": "json",
    ".yaml": "yaml",
    ".yml": "yaml",
    ".toml": "toml",
    ".xml": "xml",
    ".ini": "ini",
    ".cfg": "ini",
    ".md": "markdown",
    ".mdx": "mdx",
    ".txt": "text",
    ".rst": "rst",
    ".vue": "vue",
    ".svelte": "svelte",
    ".astro": "astro",
    ".tf": "terraform",
    ".hcl": "hcl",
    ".prisma": "prisma",
    ".proto": "protobuf",
  };

  return langMap[ext] || "text";
}

/**
 * Index a single file via the ContextStream API.
 */
async function indexFile(
  filePath: string,
  projectId: string,
  apiUrl: string,
  apiKey: string,
  projectRoot: string
): Promise<void> {
  // Read file content
  const content = fs.readFileSync(filePath, "utf-8");

  // Make path relative to project root
  const relativePath = path.relative(projectRoot, filePath);

  // Prepare request payload
  const payload = {
    files: [
      {
        path: relativePath,
        content,
        language: detectLanguage(filePath),
      },
    ],
  };

  // POST to ingest API
  const response = await fetch(`${apiUrl}/api/v1/projects/${projectId}/files/ingest`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-API-Key": apiKey,
    },
    body: JSON.stringify(payload),
    signal: AbortSignal.timeout(10000), // 10 second timeout
  });

  if (!response.ok) {
    throw new Error(`API error: ${response.status} ${response.statusText}`);
  }

  // Parse response for cooldown/rate limiting (silently skip — not an error)
  try {
    const body = (await response.json()) as { data?: { status?: string } };
    if (
      body?.data?.status === "cooldown" ||
      body?.data?.status === "daily_limit_exceeded"
    ) {
      return;
    }
  } catch {
    // Ignore JSON parse failures — response might be empty on success
  }
}

/**
 * Find project root by looking for .contextstream/config.json
 */
function findProjectRoot(filePath: string): string | null {
  let currentDir = path.dirname(path.resolve(filePath));

  for (let i = 0; i < 10; i++) {
    const configPath = path.join(currentDir, ".contextstream", "config.json");
    if (fs.existsSync(configPath)) {
      return currentDir;
    }

    const parentDir = path.dirname(currentDir);
    if (parentDir === currentDir) break;
    currentDir = parentDir;
  }

  return null;
}

/**
 * Main hook entry point.
 * Exported so it can be called from the CLI.
 */
export async function runPostWriteHook(): Promise<void> {
  // Exit early if disabled
  if (!ENABLED) {
    process.exit(0);
  }

  // Read stdin
  let inputData = "";
  for await (const chunk of process.stdin) {
    inputData += chunk;
  }

  if (!inputData.trim()) {
    process.exit(0);
  }

  let input: HookInput;
  try {
    input = JSON.parse(inputData);
  } catch {
    process.exit(0);
  }

  // Extract file path
  const filePath = extractFilePath(input);
  if (!filePath) {
    process.exit(0);
  }

  // Resolve to absolute path
  const cwd = extractCwd(input);
  const absolutePath = path.isAbsolute(filePath) ? filePath : path.resolve(cwd, filePath);

  // Check if file exists and should be indexed
  if (!fs.existsSync(absolutePath) || !shouldIndexFile(absolutePath)) {
    process.exit(0);
  }

  // Find project config
  const projectRoot = findProjectRoot(absolutePath);
  if (!projectRoot) {
    process.exit(0);
  }

  const localConfig = findLocalConfig(projectRoot);
  if (!localConfig?.project_id) {
    process.exit(0);
  }

  // Load API config
  const { apiUrl, apiKey } = loadApiConfig(projectRoot);
  if (!apiKey) {
    process.exit(0);
  }

  // Index the file (fire and forget)
  try {
    await indexFile(absolutePath, localConfig.project_id, apiUrl, apiKey, projectRoot);
  } catch {
    // Silently fail - don't block the editor
  }

  process.exit(0);
}

// Auto-run if executed directly (not imported)
// This allows both: `node post-write.js` and `import { runPostWriteHook }`
const isDirectRun = process.argv[1]?.includes("post-write") || process.argv[2] === "post-write";
if (isDirectRun) {
  runPostWriteHook().catch(() => process.exit(0));
}
