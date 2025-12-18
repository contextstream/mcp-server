import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { homedir } from 'node:os';
import { stdin, stdout } from 'node:process';
import { createInterface } from 'node:readline/promises';

import { ContextStreamClient } from './client.js';
import type { Config } from './config.js';
import { HttpError } from './http.js';
import { generateRuleContent } from './rules-templates.js';
import { VERSION } from './version.js';

type RuleMode = 'minimal' | 'full';
type InstallScope = 'global' | 'project' | 'both';
type McpScope = InstallScope | 'skip';

type EditorKey =
  | 'codex'
  | 'claude'
  | 'cursor'
  | 'windsurf'
  | 'cline'
  | 'kilo'
  | 'roo'
  | 'aider';

const EDITOR_LABELS: Record<EditorKey, string> = {
  codex: 'Codex CLI',
  claude: 'Claude Code',
  cursor: 'Cursor / VS Code',
  windsurf: 'Windsurf',
  cline: 'Cline',
  kilo: 'Kilo Code',
  roo: 'Roo Code',
  aider: 'Aider',
};

function normalizeInput(value: string): string {
  return value.trim();
}

function maskApiKey(apiKey: string): string {
  const trimmed = apiKey.trim();
  if (trimmed.length <= 8) return '********';
  return `${trimmed.slice(0, 4)}…${trimmed.slice(-4)}`;
}

function parseNumberList(input: string, max: number): number[] {
  const cleaned = input.trim().toLowerCase();
  if (!cleaned) return [];
  if (cleaned === 'all' || cleaned === '*') {
    return Array.from({ length: max }, (_, i) => i + 1);
  }
  const parts = cleaned.split(/[, ]+/).filter(Boolean);
  const out = new Set<number>();
  for (const part of parts) {
    const n = Number.parseInt(part, 10);
    if (Number.isFinite(n) && n >= 1 && n <= max) out.add(n);
  }
  return [...out].sort((a, b) => a - b);
}

async function fileExists(filePath: string): Promise<boolean> {
  try {
    await fs.stat(filePath);
    return true;
  } catch {
    return false;
  }
}

async function upsertTextFile(filePath: string, content: string, marker: string): Promise<'created' | 'appended' | 'skipped'> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);

  if (!exists) {
    await fs.writeFile(filePath, content, 'utf8');
    return 'created';
  }

  const existing = await fs.readFile(filePath, 'utf8').catch(() => '');
  if (existing.includes(marker)) return 'skipped';

  const joined = existing.trimEnd() + '\n\n' + content.trim() + '\n';
  await fs.writeFile(filePath, joined, 'utf8');
  return 'appended';
}

function globalRulesPathForEditor(editor: EditorKey): string | null {
  const home = homedir();

  switch (editor) {
    case 'codex':
      return path.join(home, '.codex', 'AGENTS.md');
    case 'claude':
      return path.join(home, '.claude', 'CLAUDE.md');
    case 'windsurf':
      return path.join(home, '.codeium', 'windsurf', 'memories', 'global_rules.md');
    case 'cline':
      return path.join(home, 'Documents', 'Cline', 'Rules', 'contextstream.md');
    case 'kilo':
      return path.join(home, '.kilocode', 'rules', 'contextstream.md');
    case 'roo':
      return path.join(home, '.roo', 'rules', 'contextstream.md');
    case 'aider':
      return path.join(home, '.aider.conf.yml');
    case 'cursor':
      // Cursor global rules are configured via the app UI; project rules are supported via `.cursorrules`.
      return null;
    default:
      return null;
  }
}

type McpServerJson = {
  command: string;
  args: string[];
  env: Record<string, string>;
};

function buildContextStreamMcpServer(params: { apiUrl: string; apiKey: string }): McpServerJson {
  return {
    command: 'npx',
    args: ['-y', '@contextstream/mcp-server'],
    env: {
      CONTEXTSTREAM_API_URL: params.apiUrl,
      CONTEXTSTREAM_API_KEY: params.apiKey,
    },
  };
}

function stripJsonComments(input: string): string {
  return (
    input
      // Remove /* */ comments
      .replace(/\/\*[\s\S]*?\*\//g, '')
      // Remove // comments
      .replace(/(^|[^:])\/\/.*$/gm, '$1')
  );
}

function tryParseJsonLike(raw: string): { ok: true; value: any } | { ok: false; error: string } {
  const trimmed = raw.replace(/^\uFEFF/, '').trim();
  if (!trimmed) return { ok: true, value: {} };

  try {
    return { ok: true, value: JSON.parse(trimmed) };
  } catch {
    // Retry with basic JSONC support.
    try {
      const noComments = stripJsonComments(trimmed);
      const noTrailingCommas = noComments.replace(/,(\s*[}\]])/g, '$1');
      return { ok: true, value: JSON.parse(noTrailingCommas) };
    } catch (err: any) {
      return { ok: false, error: err?.message || 'Invalid JSON' };
    }
  }
}

async function upsertJsonMcpConfig(filePath: string, server: McpServerJson): Promise<'created' | 'updated' | 'skipped'> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);

  let root: any = {};
  if (exists) {
    const raw = await fs.readFile(filePath, 'utf8').catch(() => '');
    const parsed = tryParseJsonLike(raw);
    if (!parsed.ok) throw new Error(`Invalid JSON in ${filePath}: ${parsed.error}`);
    root = parsed.value;
  }

  if (!root || typeof root !== 'object' || Array.isArray(root)) root = {};
  if (!root.mcpServers || typeof root.mcpServers !== 'object' || Array.isArray(root.mcpServers)) root.mcpServers = {};

  const before = JSON.stringify(root.mcpServers.contextstream ?? null);
  root.mcpServers.contextstream = server;
  const after = JSON.stringify(root.mcpServers.contextstream ?? null);

  await fs.writeFile(filePath, JSON.stringify(root, null, 2) + '\n', 'utf8');
  if (!exists) return 'created';
  return before === after ? 'skipped' : 'updated';
}

function claudeDesktopConfigPath(): string | null {
  const home = homedir();
  if (process.platform === 'darwin') {
    return path.join(home, 'Library', 'Application Support', 'Claude', 'claude_desktop_config.json');
  }
  if (process.platform === 'win32') {
    const appData = process.env.APPDATA || path.join(home, 'AppData', 'Roaming');
    return path.join(appData, 'Claude', 'claude_desktop_config.json');
  }
  return null;
}

async function upsertCodexTomlConfig(filePath: string, params: { apiUrl: string; apiKey: string }): Promise<'created' | 'updated' | 'skipped'> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);
  const existing = exists ? await fs.readFile(filePath, 'utf8').catch(() => '') : '';

  const marker = '[mcp_servers.contextstream]';
  const envMarker = '[mcp_servers.contextstream.env]';

  const block =
    `\n\n# ContextStream MCP server\n` +
    `[mcp_servers.contextstream]\n` +
    `command = "npx"\n` +
    `args = ["-y", "@contextstream/mcp-server"]\n\n` +
    `[mcp_servers.contextstream.env]\n` +
    `CONTEXTSTREAM_API_URL = "${params.apiUrl}"\n` +
    `CONTEXTSTREAM_API_KEY = "${params.apiKey}"\n`;

  if (!exists) {
    await fs.writeFile(filePath, block.trimStart(), 'utf8');
    return 'created';
  }

  if (!existing.includes(marker)) {
    await fs.writeFile(filePath, existing.trimEnd() + block, 'utf8');
    return 'updated';
  }

  if (!existing.includes(envMarker)) {
    await fs.writeFile(filePath, existing.trimEnd() + '\n\n' + envMarker + '\n' + `CONTEXTSTREAM_API_URL = "${params.apiUrl}"\n` + `CONTEXTSTREAM_API_KEY = "${params.apiKey}"\n`, 'utf8');
    return 'updated';
  }

  const lines = existing.split(/\r?\n/);
  const out: string[] = [];
  let inEnv = false;
  let sawUrl = false;
  let sawKey = false;

  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed.startsWith('[') && trimmed.endsWith(']')) {
      if (inEnv && trimmed !== envMarker) {
        if (!sawUrl) out.push(`CONTEXTSTREAM_API_URL = "${params.apiUrl}"`);
        if (!sawKey) out.push(`CONTEXTSTREAM_API_KEY = "${params.apiKey}"`);
        inEnv = false;
      }
      if (trimmed === envMarker) inEnv = true;
      out.push(line);
      continue;
    }

    if (inEnv && /^\s*CONTEXTSTREAM_API_URL\s*=/.test(line)) {
      out.push(`CONTEXTSTREAM_API_URL = "${params.apiUrl}"`);
      sawUrl = true;
      continue;
    }
    if (inEnv && /^\s*CONTEXTSTREAM_API_KEY\s*=/.test(line)) {
      out.push(`CONTEXTSTREAM_API_KEY = "${params.apiKey}"`);
      sawKey = true;
      continue;
    }
    out.push(line);
  }

  if (inEnv) {
    if (!sawUrl) out.push(`CONTEXTSTREAM_API_URL = "${params.apiUrl}"`);
    if (!sawKey) out.push(`CONTEXTSTREAM_API_KEY = "${params.apiKey}"`);
  }

  const updated = out.join('\n');
  if (updated === existing) return 'skipped';
  await fs.writeFile(filePath, updated, 'utf8');
  return 'updated';
}

async function discoverProjectsUnderFolder(parentFolder: string): Promise<string[]> {
  const entries = await fs.readdir(parentFolder, { withFileTypes: true });
  const candidates = entries
    .filter((e) => e.isDirectory() && !e.name.startsWith('.'))
    .map((e) => path.join(parentFolder, e.name));

  const projects: string[] = [];
  for (const dir of candidates) {
    const hasGit = await fileExists(path.join(dir, '.git'));
    const hasPkg = await fileExists(path.join(dir, 'package.json'));
    const hasCargo = await fileExists(path.join(dir, 'Cargo.toml'));
    const hasPyProject = await fileExists(path.join(dir, 'pyproject.toml'));
    if (hasGit || hasPkg || hasCargo || hasPyProject) projects.push(dir);
  }

  return projects;
}

function buildClientConfig(params: { apiUrl: string; apiKey?: string; jwt?: string }): Config {
  return {
    apiUrl: params.apiUrl,
    apiKey: params.apiKey,
    jwt: params.jwt,
    defaultWorkspaceId: undefined,
    defaultProjectId: undefined,
    userAgent: `contextstream-mcp/setup/${VERSION}`,
  };
}

export async function runSetupWizard(args: string[]): Promise<void> {
  const dryRun = args.includes('--dry-run');
  const rl = createInterface({ input: stdin, output: stdout });

  const writeActions: Array<{ kind: 'rules' | 'workspace-config' | 'mcp-config'; target: string; status: string }> = [];

  try {
    console.log(`ContextStream Setup Wizard (v${VERSION})`);
    console.log('This configures ContextStream MCP + rules for your AI editor(s).');
    if (dryRun) console.log('DRY RUN: no files will be written.\n');
    else console.log('');

    const apiUrlDefault = process.env.CONTEXTSTREAM_API_URL || 'https://api.contextstream.io';
    const apiUrl = normalizeInput(
      await rl.question(`ContextStream API URL [${apiUrlDefault}]: `)
    ) || apiUrlDefault;

    let apiKey = normalizeInput(process.env.CONTEXTSTREAM_API_KEY || '');

    if (apiKey) {
      const confirm = normalizeInput(
        await rl.question(`Use CONTEXTSTREAM_API_KEY from environment (${maskApiKey(apiKey)})? [Y/n]: `)
      );
      if (confirm.toLowerCase() === 'n' || confirm.toLowerCase() === 'no') apiKey = '';
    }

    if (!apiKey) {
      console.log('\nAuthentication:');
      console.log('  1) Browser login (recommended)');
      console.log('  2) Paste an API key');
      const authChoice = normalizeInput(await rl.question('Choose [1/2] (default 1): ')) || '1';

      if (authChoice === '2') {
        console.log('\nYou need a ContextStream API key to continue.');
        console.log('Create one here (then paste it): https://app.contextstream.io/settings/api-keys\n');
        apiKey = normalizeInput(await rl.question('CONTEXTSTREAM_API_KEY: '));
      } else {
        const anonClient = new ContextStreamClient(buildClientConfig({ apiUrl }));
        let device: any;
        try {
          device = await anonClient.startDeviceLogin();
        } catch (err: any) {
          const message = err instanceof HttpError ? `${err.status} ${err.code}: ${err.message}` : (err?.message || String(err));
          throw new Error(
            `Browser login is not available on this API. Please use an API key instead. (${message})`
          );
        }

        const verificationUrl =
          typeof device?.verification_uri_complete === 'string'
            ? device.verification_uri_complete
            : typeof device?.verification_uri === 'string' && typeof device?.user_code === 'string'
              ? `${device.verification_uri}?user_code=${device.user_code}`
              : undefined;

        if (!verificationUrl || typeof device?.device_code !== 'string' || typeof device?.expires_in !== 'number') {
          throw new Error('Browser login returned an unexpected response.');
        }

        console.log('\nOpen this URL to sign in and approve the setup wizard:');
        console.log(verificationUrl);
        if (typeof device?.user_code === 'string') {
          console.log(`\nCode: ${device.user_code}`);
        }
        console.log('\nWaiting for approval...');

        const startedAt = Date.now();
        const expiresMs = device.expires_in * 1000;
        const deviceCode = device.device_code as string;
        let accessToken: string | undefined;

        while (Date.now() - startedAt < expiresMs) {
          let poll: any;
          try {
            poll = await anonClient.pollDeviceLogin({ device_code: deviceCode });
          } catch (err: any) {
            const message = err instanceof HttpError ? `${err.status} ${err.code}: ${err.message}` : (err?.message || String(err));
            throw new Error(`Browser login failed while polling. (${message})`);
          }

          if (poll && poll.status === 'authorized' && typeof poll.access_token === 'string') {
            accessToken = poll.access_token;
            break;
          }

          if (poll && poll.status === 'pending') {
            const intervalSeconds = typeof poll.interval === 'number' ? poll.interval : 5;
            const waitMs = Math.max(1, intervalSeconds) * 1000;
            await new Promise((resolve) => setTimeout(resolve, waitMs));
            continue;
          }

          // Unknown response; wait briefly and retry until expiry.
          await new Promise((resolve) => setTimeout(resolve, 1000));
        }

        if (!accessToken) {
          throw new Error('Browser login expired or was not approved in time. Please run setup again.');
        }

        const jwtClient = new ContextStreamClient(buildClientConfig({ apiUrl, jwt: accessToken }));
        const keyName = `setup-wizard-${Date.now()}`;
        let createdKey: any;
        try {
          createdKey = await jwtClient.createApiKey({ name: keyName });
        } catch (err: any) {
          const message = err instanceof HttpError ? `${err.status} ${err.code}: ${err.message}` : (err?.message || String(err));
          throw new Error(`Login succeeded but API key creation failed. (${message})`);
        }

        if (typeof createdKey?.secret_key !== 'string' || !createdKey.secret_key.trim()) {
          throw new Error('API key creation returned an unexpected response.');
        }

        apiKey = createdKey.secret_key.trim();
        console.log(`\nCreated API key: ${maskApiKey(apiKey)}\n`);
      }
    }

    const client = new ContextStreamClient(buildClientConfig({ apiUrl, apiKey }));

    // Validate auth
    let me: any;
    try {
      me = await client.me();
    } catch (err: any) {
      const message = err instanceof HttpError ? `${err.status} ${err.code}: ${err.message}` : (err?.message || String(err));
      throw new Error(`Authentication failed. Check your API key. (${message})`);
    }

    const email =
      typeof me?.data?.email === 'string'
        ? me.data.email
        : typeof me?.email === 'string'
          ? me.email
          : undefined;

    console.log(`Authenticated as: ${email || 'unknown user'} (${maskApiKey(apiKey)})\n`);

    // Workspace selection
    let workspaceId: string | undefined;
    let workspaceName: string | undefined;

    console.log('Workspace setup:');
    console.log('  1) Create a new workspace');
    console.log('  2) Select an existing workspace');
    console.log('  3) Skip (rules only, no workspace mapping)');
    const wsChoice = normalizeInput(await rl.question('Choose [1/2/3] (default 2): ')) || '2';

    if (wsChoice === '1') {
      const name = normalizeInput(await rl.question('Workspace name: '));
      if (!name) throw new Error('Workspace name is required.');
      const description = normalizeInput(await rl.question('Workspace description (optional): '));
      let visibility = 'private';
      while (true) {
        const raw = normalizeInput(await rl.question('Visibility [private/team/org] (default private): ')) || 'private';
        const normalized = raw.trim().toLowerCase() === 'public' ? 'org' : raw.trim().toLowerCase();
        if (normalized === 'private' || normalized === 'team' || normalized === 'org') {
          visibility = normalized;
          break;
        }
        console.log('Invalid visibility. Choose: private, team, org.');
      }

      if (!dryRun) {
        const created = (await client.createWorkspace({ name, description: description || undefined, visibility })) as any;
        workspaceId = typeof created?.id === 'string' ? created.id : undefined;
        workspaceName = typeof created?.name === 'string' ? created.name : name;
      } else {
        workspaceId = 'dry-run';
        workspaceName = name;
      }

      console.log(`Workspace: ${workspaceName}${workspaceId ? ` (${workspaceId})` : ''}\n`);
    } else if (wsChoice === '2') {
      const list = await client.listWorkspaces({ page_size: 50 }) as any;
      const items: Array<{ id?: string; name?: string; description?: string }> =
        Array.isArray(list?.items) ? list.items :
        Array.isArray(list?.data?.items) ? list.data.items :
        [];

      if (items.length === 0) {
        console.log('No workspaces found. Creating a new one is recommended.\n');
      } else {
        items.slice(0, 20).forEach((w, i) => {
          console.log(`  ${i + 1}) ${w.name || 'Untitled'}${w.id ? ` (${w.id})` : ''}`);
        });
        const idxRaw = normalizeInput(await rl.question('Select workspace number (or blank to skip): '));
        if (idxRaw) {
          const idx = Number.parseInt(idxRaw, 10);
          const selected = Number.isFinite(idx) ? items[idx - 1] : undefined;
          if (selected?.id) {
            workspaceId = selected.id;
            workspaceName = selected.name;
          }
        }
      }
    }

    // Rules mode + editors
    console.log('Rule verbosity:');
    console.log('  1) Minimal (recommended)');
    console.log('  2) Full (more context and guidance, more tokens)');
    const modeChoice = normalizeInput(await rl.question('Choose [1/2] (default 1): ')) || '1';
    const mode: RuleMode = modeChoice === '2' ? 'full' : 'minimal';

    const editors: EditorKey[] = ['codex', 'claude', 'cursor', 'windsurf', 'cline', 'kilo', 'roo', 'aider'];
    console.log('\nSelect editors to configure (comma-separated numbers, or "all"):');
    editors.forEach((e, i) => console.log(`  ${i + 1}) ${EDITOR_LABELS[e]}`));
    const selectedRaw = normalizeInput(await rl.question('Editors [all]: ')) || 'all';
    const selectedNums = parseNumberList(selectedRaw, editors.length);
    const selectedEditors = selectedNums.length ? selectedNums.map((n) => editors[n - 1]) : editors;

    console.log('\nInstall rules as:');
    console.log('  1) Global');
    console.log('  2) Project');
    console.log('  3) Both');
    const scopeChoice = normalizeInput(await rl.question('Choose [1/2/3] (default 3): ')) || '3';
    const scope: InstallScope = scopeChoice === '1' ? 'global' : scopeChoice === '2' ? 'project' : 'both';

    console.log('\nInstall MCP server config as:');
    console.log('  1) Global');
    console.log('  2) Project');
    console.log('  3) Both');
    console.log('  4) Skip (rules only)');
    const mcpChoice = normalizeInput(await rl.question('Choose [1/2/3/4] (default 3): ')) || '3';
    const mcpScope: McpScope =
      mcpChoice === '4'
        ? 'skip'
        : mcpChoice === '1'
          ? 'global'
          : mcpChoice === '2'
            ? 'project'
            : 'both';

    const mcpServer = buildContextStreamMcpServer({ apiUrl, apiKey });

    // Global MCP config
    if (mcpScope === 'global' || mcpScope === 'both') {
      console.log('\nInstalling global MCP config...');
      for (const editor of selectedEditors) {
        try {
          if (editor === 'codex') {
            const filePath = path.join(homedir(), '.codex', 'config.toml');
            if (dryRun) {
              writeActions.push({ kind: 'mcp-config', target: filePath, status: 'dry-run' });
              console.log(`- ${EDITOR_LABELS[editor]}: would update ${filePath}`);
              continue;
            }
            const status = await upsertCodexTomlConfig(filePath, { apiUrl, apiKey });
            writeActions.push({ kind: 'mcp-config', target: filePath, status });
            console.log(`- ${EDITOR_LABELS[editor]}: ${status} ${filePath}`);
            continue;
          }

          if (editor === 'windsurf') {
            const filePath = path.join(homedir(), '.codeium', 'windsurf', 'mcp_config.json');
            if (dryRun) {
              writeActions.push({ kind: 'mcp-config', target: filePath, status: 'dry-run' });
              console.log(`- ${EDITOR_LABELS[editor]}: would update ${filePath}`);
              continue;
            }
            const status = await upsertJsonMcpConfig(filePath, mcpServer);
            writeActions.push({ kind: 'mcp-config', target: filePath, status });
            console.log(`- ${EDITOR_LABELS[editor]}: ${status} ${filePath}`);
            continue;
          }

          if (editor === 'claude') {
            const desktopPath = claudeDesktopConfigPath();
            if (desktopPath) {
              const useDesktop =
                normalizeInput(await rl.question('Also configure Claude Desktop (GUI app)? [y/N]: ')).toLowerCase() === 'y';
              if (useDesktop) {
                if (dryRun) {
                  writeActions.push({ kind: 'mcp-config', target: desktopPath, status: 'dry-run' });
                  console.log(`- Claude Desktop: would update ${desktopPath}`);
                } else {
                  const status = await upsertJsonMcpConfig(desktopPath, mcpServer);
                  writeActions.push({ kind: 'mcp-config', target: desktopPath, status });
                  console.log(`- Claude Desktop: ${status} ${desktopPath}`);
                }
              }
            }

            console.log('- Claude Code: global MCP config is best done via `claude mcp add --scope user ...` (see docs).');
            continue;
          }

          if (editor === 'cursor') {
            console.log(`- ${EDITOR_LABELS[editor]}: MCP config is project-based (skipping global).`);
            continue;
          }
          if (editor === 'cline') {
            console.log(`- ${EDITOR_LABELS[editor]}: MCP config is managed via the extension UI (skipping global).`);
            continue;
          }
          if (editor === 'kilo' || editor === 'roo') {
            console.log(`- ${EDITOR_LABELS[editor]}: project MCP config supported via file; global is managed via the app UI.`);
            continue;
          }
          if (editor === 'aider') {
            console.log(`- ${EDITOR_LABELS[editor]}: no MCP config file to write (rules only).`);
            continue;
          }
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          console.log(`- ${EDITOR_LABELS[editor]}: failed to write MCP config: ${message}`);
        }
      }
    }

    // Global rules
    if (scope === 'global' || scope === 'both') {
      console.log('\nInstalling global rules...');
      for (const editor of selectedEditors) {
        const filePath = globalRulesPathForEditor(editor);
        if (!filePath) {
          console.log(`- ${EDITOR_LABELS[editor]}: global rules need manual setup (project rules supported).`);
          continue;
        }

        const rule = generateRuleContent(editor, {
          workspaceName,
          workspaceId: workspaceId && workspaceId !== 'dry-run' ? workspaceId : undefined,
          mode,
        });
        if (!rule) continue;

        if (dryRun) {
          writeActions.push({ kind: 'rules', target: filePath, status: 'dry-run' });
          console.log(`- ${EDITOR_LABELS[editor]}: would write ${filePath}`);
          continue;
        }

        const status = await upsertTextFile(filePath, rule.content, 'ContextStream');
        writeActions.push({ kind: 'rules', target: filePath, status });
        console.log(`- ${EDITOR_LABELS[editor]}: ${status} ${filePath}`);
      }
    }

    // Project rules + workspace mapping
    const projectPaths = new Set<string>();
    const needsProjects = scope === 'project' || scope === 'both' || mcpScope === 'project' || mcpScope === 'both';

    if (needsProjects) {
      console.log('\nProject setup...');

      const addCwd = normalizeInput(await rl.question(`Add current folder as a project? [Y/n] (${process.cwd()}): `));
      if (addCwd.toLowerCase() !== 'n' && addCwd.toLowerCase() !== 'no') {
        projectPaths.add(path.resolve(process.cwd()));
      }

      while (true) {
        console.log('\n  1) Add another project path');
        console.log('  2) Add all projects under a folder');
        console.log('  3) Continue');
        const choice = normalizeInput(await rl.question('Choose [1/2/3] (default 3): ')) || '3';
        if (choice === '3') break;

        if (choice === '1') {
          const p = normalizeInput(await rl.question('Project folder path: '));
          if (p) projectPaths.add(path.resolve(p));
          continue;
        }

        if (choice === '2') {
          const parent = normalizeInput(await rl.question('Parent folder path: '));
          if (!parent) continue;
          const parentAbs = path.resolve(parent);
          const projects = await discoverProjectsUnderFolder(parentAbs);
          if (projects.length === 0) {
            console.log(`No projects detected under ${parentAbs} (looked for .git/package.json/Cargo.toml/pyproject.toml).`);
            continue;
          }
          console.log(`Found ${projects.length} project(s):`);
          projects.slice(0, 25).forEach((p) => console.log(`- ${p}`));
          if (projects.length > 25) console.log(`…and ${projects.length - 25} more`);

          const confirm = normalizeInput(await rl.question('Add these projects? [Y/n]: '));
          if (confirm.toLowerCase() === 'n' || confirm.toLowerCase() === 'no') continue;
          projects.forEach((p) => projectPaths.add(p));
        }
      }
    }

    const projects = [...projectPaths];
    if (projects.length && needsProjects) {
      console.log(`\nApplying to ${projects.length} project(s)...`);
    }

    const createParentMapping =
      !!workspaceId &&
      workspaceId !== 'dry-run' &&
      projects.length > 1 &&
      (normalizeInput(await rl.question('Also create a parent folder mapping for auto-detection? [y/N]: ')).toLowerCase() === 'y');

    for (const projectPath of projects) {
      // Workspace association per project (writes .contextstream/config.json)
      if (workspaceId && workspaceId !== 'dry-run' && workspaceName && !dryRun) {
        try {
          await client.associateWorkspace({
            folder_path: projectPath,
            workspace_id: workspaceId,
            workspace_name: workspaceName,
            create_parent_mapping: createParentMapping,
          });
          writeActions.push({ kind: 'workspace-config', target: path.join(projectPath, '.contextstream', 'config.json'), status: 'created' });
          console.log(`- Linked workspace in ${projectPath}`);
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          console.log(`- Failed to link workspace in ${projectPath}: ${message}`);
        }
      } else if (workspaceId && workspaceId !== 'dry-run' && workspaceName && dryRun) {
        writeActions.push({ kind: 'workspace-config', target: path.join(projectPath, '.contextstream', 'config.json'), status: 'dry-run' });
      }

      // Project MCP configs per editor
      if (mcpScope === 'project' || mcpScope === 'both') {
        for (const editor of selectedEditors) {
          try {
            if (editor === 'cursor') {
              const cursorPath = path.join(projectPath, '.cursor', 'mcp.json');
              const vscodePath = path.join(projectPath, '.vscode', 'mcp.json');
              if (dryRun) {
                writeActions.push({ kind: 'mcp-config', target: cursorPath, status: 'dry-run' });
                writeActions.push({ kind: 'mcp-config', target: vscodePath, status: 'dry-run' });
              } else {
                const status1 = await upsertJsonMcpConfig(cursorPath, mcpServer);
                const status2 = await upsertJsonMcpConfig(vscodePath, mcpServer);
                writeActions.push({ kind: 'mcp-config', target: cursorPath, status: status1 });
                writeActions.push({ kind: 'mcp-config', target: vscodePath, status: status2 });
              }
              continue;
            }

            if (editor === 'claude') {
              const mcpPath = path.join(projectPath, '.mcp.json');
              if (dryRun) {
                writeActions.push({ kind: 'mcp-config', target: mcpPath, status: 'dry-run' });
              } else {
                const status = await upsertJsonMcpConfig(mcpPath, mcpServer);
                writeActions.push({ kind: 'mcp-config', target: mcpPath, status });
              }
              continue;
            }

            if (editor === 'kilo') {
              const kiloPath = path.join(projectPath, '.kilocode', 'mcp.json');
              if (dryRun) {
                writeActions.push({ kind: 'mcp-config', target: kiloPath, status: 'dry-run' });
              } else {
                const status = await upsertJsonMcpConfig(kiloPath, mcpServer);
                writeActions.push({ kind: 'mcp-config', target: kiloPath, status });
              }
              continue;
            }

            if (editor === 'roo') {
              const rooPath = path.join(projectPath, '.roo', 'mcp.json');
              if (dryRun) {
                writeActions.push({ kind: 'mcp-config', target: rooPath, status: 'dry-run' });
              } else {
                const status = await upsertJsonMcpConfig(rooPath, mcpServer);
                writeActions.push({ kind: 'mcp-config', target: rooPath, status });
              }
              continue;
            }
          } catch (err: any) {
            const message = err instanceof Error ? err.message : String(err);
            console.log(`- Failed to write MCP config for ${EDITOR_LABELS[editor]} in ${projectPath}: ${message}`);
          }
        }
      }

      // Project rules per editor
      for (const editor of selectedEditors) {
        if (scope !== 'project' && scope !== 'both') continue;
        const rule = generateRuleContent(editor, {
          workspaceName,
          workspaceId: workspaceId && workspaceId !== 'dry-run' ? workspaceId : undefined,
          projectName: path.basename(projectPath),
          mode,
        });
        if (!rule) continue;

        const filePath = path.join(projectPath, rule.filename);
        if (dryRun) {
          writeActions.push({ kind: 'rules', target: filePath, status: 'dry-run' });
          continue;
        }
        try {
          const status = await upsertTextFile(filePath, rule.content, 'ContextStream');
          writeActions.push({ kind: 'rules', target: filePath, status });
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          writeActions.push({ kind: 'rules', target: filePath, status: `error: ${message}` });
        }
      }
    }

    console.log('\nDone.');
    if (writeActions.length) {
      const created = writeActions.filter((a) => a.status === 'created').length;
      const appended = writeActions.filter((a) => a.status === 'appended').length;
      const updated = writeActions.filter((a) => a.status === 'updated').length;
      const skipped = writeActions.filter((a) => a.status === 'skipped').length;
      const dry = writeActions.filter((a) => a.status === 'dry-run').length;
      console.log(`Summary: ${created} created, ${updated} updated, ${appended} appended, ${skipped} skipped, ${dry} dry-run.`);
    }

    console.log('\nNext steps:');
    console.log('- Restart your editor/CLI after changing MCP config or rules.');
    console.log('- If any tools require UI-based MCP setup (e.g. Cline/Kilo/Roo global), follow https://contextstream.io/docs/mcp.');
  } finally {
    rl.close();
  }
}
