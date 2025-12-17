import { ResourceTemplate } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { ContextStreamClient } from './client.js';

function wrapText(uri: string, text: string) {
  return { contents: [{ uri, text }] };
}

export function registerResources(server: McpServer, client: ContextStreamClient, apiUrl: string) {
  // OpenAPI resource
  server.registerResource(
    'contextstream-openapi',
    new ResourceTemplate('contextstream:openapi', { list: undefined }),
    {
      title: 'ContextStream OpenAPI spec',
      description: 'Machine-readable OpenAPI from the configured API endpoint',
      mimeType: 'application/json',
    },
    async () => {
      const uri = `${apiUrl.replace(/\/$/, '')}/api-docs/openapi.json`;
      const res = await fetch(uri);
      const text = await res.text();
      return wrapText('contextstream:openapi', text);
    }
  );

  // Workspaces list resource
  server.registerResource(
    'contextstream-workspaces',
    new ResourceTemplate('contextstream:workspaces', { list: undefined }),
    { title: 'Workspaces', description: 'List of accessible workspaces' },
    async () => {
      const data = await client.listWorkspaces();
      return wrapText('contextstream:workspaces', JSON.stringify(data, null, 2));
    }
  );

  // Projects by workspace resource template
  server.registerResource(
    'contextstream-projects',
    new ResourceTemplate('contextstream:projects/{workspaceId}', { list: undefined }),
    { title: 'Projects for workspace', description: 'Projects in the specified workspace' },
    async (uri: URL, { workspaceId }: { workspaceId: string | string[] }) => {
      const wsId = Array.isArray(workspaceId) ? workspaceId[0] : workspaceId;
      const data = await client.listProjects({ workspace_id: wsId });
      return wrapText(uri.href, JSON.stringify(data, null, 2));
    }
  );
}
