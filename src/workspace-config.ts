import * as fs from "fs";
import * as path from "path";

export interface WorkspaceConfig {
  workspace_id: string;
  workspace_name?: string;
  project_id?: string;
  project_name?: string;
  associated_at?: string;
  // Added for version tracking and config consistency with desktop app
  version?: string;
  configured_editors?: string[];
  context_pack?: boolean;
  api_url?: string;
  updated_at?: string;
  // User preference for auto-indexing (set during setup wizard)
  indexing_enabled?: boolean;
}

export interface ParentMapping {
  pattern: string; // e.g., "/home/user/dev/projects/*"
  workspace_id: string;
  workspace_name: string;
}

const CONFIG_DIR = ".contextstream";
const CONFIG_FILE = "config.json";
const GLOBAL_MAPPINGS_FILE = ".contextstream-mappings.json";

/**
 * Read workspace config from a repo's .contextstream/config.json
 */
export function readLocalConfig(repoPath: string): WorkspaceConfig | null {
  const configPath = path.join(repoPath, CONFIG_DIR, CONFIG_FILE);
  try {
    if (fs.existsSync(configPath)) {
      const content = fs.readFileSync(configPath, "utf-8");
      return JSON.parse(content) as WorkspaceConfig;
    }
  } catch (e) {
    console.error(`Failed to read config from ${configPath}:`, e);
  }
  return null;
}

/**
 * Write workspace config to a repo's .contextstream/config.json
 */
export function writeLocalConfig(repoPath: string, config: WorkspaceConfig): boolean {
  const configDir = path.join(repoPath, CONFIG_DIR);
  const configPath = path.join(configDir, CONFIG_FILE);
  try {
    if (!fs.existsSync(configDir)) {
      fs.mkdirSync(configDir, { recursive: true });
    }
    fs.writeFileSync(configPath, JSON.stringify(config, null, 2));
    return true;
  } catch (e) {
    console.error(`Failed to write config to ${configPath}:`, e);
    return false;
  }
}

/**
 * Read global parent folder mappings from user's home directory
 */
export function readGlobalMappings(): ParentMapping[] {
  const homeDir = process.env.HOME || process.env.USERPROFILE || "";
  const mappingsPath = path.join(homeDir, GLOBAL_MAPPINGS_FILE);
  try {
    if (fs.existsSync(mappingsPath)) {
      const content = fs.readFileSync(mappingsPath, "utf-8");
      return JSON.parse(content) as ParentMapping[];
    }
  } catch (e) {
    console.error(`Failed to read global mappings:`, e);
  }
  return [];
}

/**
 * Write global parent folder mappings to user's home directory
 */
export function writeGlobalMappings(mappings: ParentMapping[]): boolean {
  const homeDir = process.env.HOME || process.env.USERPROFILE || "";
  const mappingsPath = path.join(homeDir, GLOBAL_MAPPINGS_FILE);
  try {
    fs.writeFileSync(mappingsPath, JSON.stringify(mappings, null, 2));
    return true;
  } catch (e) {
    console.error(`Failed to write global mappings:`, e);
    return false;
  }
}

/**
 * Add a new parent folder mapping
 */
export function addGlobalMapping(mapping: ParentMapping): boolean {
  const normalizedPattern = path.normalize(mapping.pattern);
  const mappings = readGlobalMappings();
  // Remove any existing mapping with same pattern
  const filtered = mappings.filter((m) => path.normalize(m.pattern) !== normalizedPattern);
  filtered.push({ ...mapping, pattern: normalizedPattern });
  return writeGlobalMappings(filtered);
}

/**
 * Check if a repo path matches any parent folder mapping
 */
export function findMatchingMapping(repoPath: string): ParentMapping | null {
  const mappings = readGlobalMappings();
  const normalizedRepo = path.normalize(repoPath);

  for (const mapping of mappings) {
    const normalizedPattern = path.normalize(mapping.pattern);

    // Handle wildcard patterns like "/home/user/dev/projects/*" (or "C:\\dev\\projects\\*")
    if (normalizedPattern.endsWith(`${path.sep}*`)) {
      const parentDir = normalizedPattern.slice(0, -2);
      if (normalizedRepo.startsWith(parentDir + path.sep)) {
        return mapping;
      }
    } else if (normalizedRepo === normalizedPattern) {
      return mapping;
    }
  }
  return null;
}

/**
 * Resolve workspace for a given repo path using the discovery chain:
 * 1. Local .contextstream/config.json
 * 2. Parent folder heuristic mappings
 * 3. Return null (ambiguous - needs user selection)
 */
export function resolveWorkspace(repoPath: string): {
  config: WorkspaceConfig | null;
  source: "local_config" | "parent_mapping" | "ambiguous";
} {
  // Step 1: Check local config
  const localConfig = readLocalConfig(repoPath);
  if (localConfig) {
    return { config: localConfig, source: "local_config" };
  }

  // Step 2: Check parent folder mappings
  const mapping = findMatchingMapping(repoPath);
  if (mapping) {
    return {
      config: {
        workspace_id: mapping.workspace_id,
        workspace_name: mapping.workspace_name,
      },
      source: "parent_mapping",
    };
  }

  // Step 3: Ambiguous - needs user selection
  return { config: null, source: "ambiguous" };
}
