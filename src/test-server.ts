/**
 * MCP Test Server
 *
 * A lightweight HTTP server that wraps the MCP server for testing purposes.
 * This allows the admin tools page to test MCP-only tools via HTTP.
 *
 * Usage:
 *   CONTEXTSTREAM_API_KEY="your_key" npx tsx src/test-server.ts
 *
 * Endpoints:
 *   POST /test-tool - Execute an MCP tool
 *     Body: { "tool": "session_init", "arguments": { ... } }
 *   GET /health - Health check
 *   GET /tools - List available tools
 */

import { createServer, IncomingMessage, ServerResponse } from 'http';
import { spawn, ChildProcess } from 'child_process';
import { createInterface, Interface } from 'readline';
import { loadConfig } from './config.js';
import { VERSION } from './version.js';

const PORT = parseInt(process.env.MCP_TEST_PORT || '3099', 10);

interface JsonRpcRequest {
  jsonrpc: '2.0';
  id: number;
  method: string;
  params?: unknown;
}

interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number;
  result?: unknown;
  error?: {
    code: number;
    message: string;
    data?: unknown;
  };
}

// Pending requests map
const pendingRequests = new Map<number, {
  resolve: (value: JsonRpcResponse) => void;
  reject: (error: Error) => void;
}>();

let requestId = 0;
let mcpProcess: ChildProcess | null = null;
let mcpReadline: Interface | null = null;
let initialized = false;

/**
 * Start the MCP server as a subprocess
 */
async function startMcpServer(): Promise<void> {
  const config = loadConfig();

  console.log('[MCP Test Server] Starting MCP subprocess...');
  console.log(`[MCP Test Server] API URL: ${config.apiUrl}`);
  console.log(`[MCP Test Server] Auth: ${config.apiKey ? 'API Key' : config.jwt ? 'JWT' : 'None'}`);

  mcpProcess = spawn('node', ['dist/index.js'], {
    cwd: process.cwd(),
    env: {
      ...process.env,
      // Pass through the config
      CONTEXTSTREAM_API_URL: config.apiUrl,
      CONTEXTSTREAM_API_KEY: config.apiKey || '',
      CONTEXTSTREAM_JWT: config.jwt || '',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  // Handle stderr (logs)
  mcpProcess.stderr?.on('data', (data: Buffer) => {
    const msg = data.toString().trim();
    if (msg) {
      console.log(`[MCP] ${msg}`);
    }
  });

  // Handle stdout (JSON-RPC responses)
  mcpReadline = createInterface({
    input: mcpProcess.stdout!,
    terminal: false,
  });

  mcpReadline.on('line', (line: string) => {
    try {
      const response = JSON.parse(line) as JsonRpcResponse;
      const pending = pendingRequests.get(response.id);
      if (pending) {
        pendingRequests.delete(response.id);
        pending.resolve(response);
      }
    } catch (e) {
      // Ignore non-JSON lines
    }
  });

  mcpProcess.on('error', (err: Error) => {
    console.error('[MCP Test Server] MCP process error:', err);
  });

  mcpProcess.on('exit', (code: number | null) => {
    console.log(`[MCP Test Server] MCP process exited with code ${code}`);
    mcpProcess = null;
    initialized = false;
  });

  // Wait for MCP server to be ready
  await new Promise<void>((resolve) => setTimeout(resolve, 500));

  // Initialize the MCP session
  await initializeMcp();
}

/**
 * Send a JSON-RPC request to the MCP server
 */
async function sendRequest(method: string, params?: unknown): Promise<JsonRpcResponse> {
  if (!mcpProcess || !mcpProcess.stdin) {
    throw new Error('MCP process not running');
  }

  const id = ++requestId;
  const request: JsonRpcRequest = {
    jsonrpc: '2.0',
    id,
    method,
    params,
  };

  return new Promise((resolve, reject) => {
    pendingRequests.set(id, { resolve, reject });

    const timeoutId = setTimeout(() => {
      pendingRequests.delete(id);
      reject(new Error('Request timeout'));
    }, 30000);

    mcpProcess!.stdin!.write(JSON.stringify(request) + '\n', (err: Error | null | undefined) => {
      if (err) {
        clearTimeout(timeoutId);
        pendingRequests.delete(id);
        reject(err);
      }
    });

    // Clear timeout on success
    pendingRequests.get(id)!.resolve = (value: JsonRpcResponse) => {
      clearTimeout(timeoutId);
      resolve(value);
    };
  });
}

/**
 * Initialize the MCP connection
 */
async function initializeMcp(): Promise<void> {
  console.log('[MCP Test Server] Initializing MCP connection...');

  const response = await sendRequest('initialize', {
    protocolVersion: '2024-11-05',
    capabilities: {},
    clientInfo: {
      name: 'mcp-test-server',
      version: VERSION,
    },
  });

  if (response.error) {
    throw new Error(`MCP initialization failed: ${response.error.message}`);
  }

  // Send initialized notification
  if (mcpProcess?.stdin) {
    mcpProcess.stdin.write(JSON.stringify({
      jsonrpc: '2.0',
      method: 'notifications/initialized',
    }) + '\n');
  }

  initialized = true;
  console.log('[MCP Test Server] MCP connection initialized');
}

/**
 * Call an MCP tool
 */
async function callTool(toolName: string, args: Record<string, unknown>): Promise<unknown> {
  if (!initialized) {
    await initializeMcp();
  }

  const response = await sendRequest('tools/call', {
    name: toolName,
    arguments: args,
  });

  if (response.error) {
    throw new Error(response.error.message);
  }

  return response.result;
}

/**
 * List available tools
 */
async function listTools(): Promise<unknown> {
  if (!initialized) {
    await initializeMcp();
  }

  const response = await sendRequest('tools/list', {});

  if (response.error) {
    throw new Error(response.error.message);
  }

  return response.result;
}

/**
 * Parse request body
 */
async function parseBody(req: IncomingMessage): Promise<unknown> {
  return new Promise((resolve, reject) => {
    let body = '';
    req.on('data', (chunk: Buffer) => {
      body += chunk.toString();
    });
    req.on('end', () => {
      try {
        resolve(body ? JSON.parse(body) : {});
      } catch (e) {
        reject(new Error('Invalid JSON'));
      }
    });
    req.on('error', reject);
  });
}

/**
 * Send JSON response
 */
function sendJson(res: ServerResponse, status: number, data: unknown): void {
  res.writeHead(status, {
    'Content-Type': 'application/json',
    'Access-Control-Allow-Origin': '*',
    'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
    'Access-Control-Allow-Headers': 'Content-Type, Authorization',
  });
  res.end(JSON.stringify(data));
}

/**
 * HTTP request handler
 */
async function handleRequest(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const url = new URL(req.url || '/', `http://localhost:${PORT}`);

  // Handle CORS preflight
  if (req.method === 'OPTIONS') {
    res.writeHead(204, {
      'Access-Control-Allow-Origin': '*',
      'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
      'Access-Control-Allow-Headers': 'Content-Type, Authorization',
    });
    res.end();
    return;
  }

  try {
    // Health check
    if (url.pathname === '/health') {
      sendJson(res, 200, {
        status: 'ok',
        version: VERSION,
        mcpRunning: mcpProcess !== null,
        initialized,
      });
      return;
    }

    // List tools
    if (url.pathname === '/tools' && req.method === 'GET') {
      const tools = await listTools();
      sendJson(res, 200, { success: true, data: tools });
      return;
    }

    // Test tool
    if (url.pathname === '/test-tool' && req.method === 'POST') {
      const body = await parseBody(req) as { tool?: string; arguments?: Record<string, unknown> };

      if (!body.tool) {
        sendJson(res, 400, { success: false, error: 'Missing tool name' });
        return;
      }

      const start = Date.now();
      const result = await callTool(body.tool, body.arguments || {});
      const responseTime = Date.now() - start;

      sendJson(res, 200, {
        success: true,
        data: result,
        responseTime,
        tool: body.tool,
      });
      return;
    }

    // Not found
    sendJson(res, 404, { success: false, error: 'Not found' });
  } catch (error: unknown) {
    const err = error as Error;
    console.error('[MCP Test Server] Error:', err.message);
    sendJson(res, 500, {
      success: false,
      error: err.message,
    });
  }
}

/**
 * Main entry point
 */
async function main(): Promise<void> {
  console.log(`[MCP Test Server] Starting on port ${PORT}...`);

  // Start MCP server subprocess
  await startMcpServer();

  // Create HTTP server
  const server = createServer(handleRequest);

  server.listen(PORT, '0.0.0.0', () => {
    console.log(`[MCP Test Server] Listening on http://0.0.0.0:${PORT}`);
    console.log(`[MCP Test Server] Endpoints:`);
    console.log(`  GET  /health     - Health check`);
    console.log(`  GET  /tools      - List available tools`);
    console.log(`  POST /test-tool  - Execute a tool`);
  });

  // Handle shutdown
  process.on('SIGINT', () => {
    console.log('\n[MCP Test Server] Shutting down...');
    if (mcpProcess) {
      mcpProcess.kill();
    }
    server.close();
    process.exit(0);
  });
}

main().catch((err) => {
  console.error('[MCP Test Server] Fatal error:', err);
  process.exit(1);
});
