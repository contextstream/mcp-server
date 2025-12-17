declare module '@modelcontextprotocol/sdk/server/mcp.js' {
  export class ResourceTemplate {
    constructor(uriTemplate: string, options?: unknown);
  }

  export class McpServer {
    constructor(info: { name: string; version: string });

    // Prompts
    registerPrompt(name: string, config: unknown, cb: (...args: any[]) => Promise<unknown> | unknown): unknown;

    // Tools
    registerTool(name: string, config: unknown, handler: (input: any) => Promise<any> | any): unknown;

    // Resources
    registerResource(
      name: string,
      uriOrTemplate: unknown,
      metadata: unknown,
      readCallback: (...args: any[]) => Promise<unknown> | unknown
    ): unknown;

    connect(transport: unknown): Promise<void>;
    close(): Promise<void>;

    // Low-level protocol server (capabilities, roots, etc.)
    server: any;
  }
}

declare module '@modelcontextprotocol/sdk/server/stdio.js' {
  export class StdioServerTransport {
    constructor();
  }
}

