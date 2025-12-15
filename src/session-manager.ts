import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { ContextStreamClient } from './client.js';

/**
 * SessionManager tracks auto-context state per MCP connection.
 * 
 * This enables the "First-Tool Interceptor" pattern:
 * - On the FIRST tool call of any session, auto-initialize context
 * - Prepend context summary to the tool response
 * - Subsequent calls skip auto-init (context already loaded)
 * 
 * This works across ALL MCP clients (Windsurf, Cursor, Claude Desktop, VS Code, etc.)
 * because it only relies on the Tools primitive - the universal MCP feature.
 */
export class SessionManager {
  private initialized = false;
  private initializationPromise: Promise<unknown> | null = null;
  private context: Record<string, unknown> | null = null;
  private ideRoots: string[] = [];
  private folderPath: string | null = null;
  private contextSmartCalled = false;
  private warningShown = false;

  constructor(
    private server: McpServer,
    private client: ContextStreamClient
  ) {}

  /**
   * Check if session has been auto-initialized
   */
  isInitialized(): boolean {
    return this.initialized;
  }

  /**
   * Get the auto-loaded context (if any)
   */
  getContext(): Record<string, unknown> | null {
    return this.context;
  }

  /**
   * Mark session as manually initialized (e.g., when session_init is called explicitly)
   */
  markInitialized(context: Record<string, unknown>) {
    this.initialized = true;
    this.context = context;

    // Promote resolved workspace/project to client defaults so subsequent calls
    // (including those without explicit workspace_id in payload/path/query)
    // can still send X-Workspace-Id for workspace-pooled rate limits.
    const workspaceId = typeof context.workspace_id === 'string' ? (context.workspace_id as string) : undefined;
    const projectId = typeof context.project_id === 'string' ? (context.project_id as string) : undefined;
    if (workspaceId || projectId) {
      this.client.setDefaults({ workspace_id: workspaceId, project_id: projectId });
    }
  }

  /**
   * Set the folder path hint (can be passed from tools that know the workspace path)
   */
  setFolderPath(path: string) {
    this.folderPath = path;
  }

  /**
   * Mark that context_smart has been called in this session
   */
  markContextSmartCalled() {
    this.contextSmartCalled = true;
  }

  /**
   * Check if context_smart has been called and warn if not.
   * Returns true if a warning was shown, false otherwise.
   */
  warnIfContextSmartNotCalled(toolName: string): boolean {
    // Skip warning for these tools
    const skipWarningTools = ['session_init', 'context_smart', 'session_recall', 'session_remember'];
    if (skipWarningTools.includes(toolName)) {
      return false;
    }

    // Only warn once per session and only if session is initialized
    if (!this.initialized || this.contextSmartCalled || this.warningShown) {
      return false;
    }

    this.warningShown = true;
    console.warn(`[ContextStream] Warning: ${toolName} called without context_smart.`);
    console.warn('[ContextStream] For best results, call context_smart(user_message="...") before other tools.');
    console.warn('[ContextStream] context_smart provides semantically relevant context for the user\'s query.');
    return true;
  }

  /**
   * Auto-initialize the session if not already done.
   * Returns context summary to prepend to tool response.
   * 
   * This is the core of the auto-context feature.
   */
  async autoInitialize(): Promise<{ contextSummary: string; context: Record<string, unknown> } | null> {
    // Already initialized - no need to do anything
    if (this.initialized) {
      return null;
    }

    // Prevent concurrent initialization attempts
    if (this.initializationPromise) {
      await this.initializationPromise;
      return null;
    }

    // Try multiple methods to detect workspace path
    
    // Method 1: Check client capabilities and call listRoots if supported
    try {
      const capabilities = this.server.server.getClientCapabilities();
      console.error('[ContextStream] Client capabilities:', JSON.stringify(capabilities));
      
      if (capabilities?.roots) {
        const rootsResponse = await this.server.server.listRoots();
        console.error('[ContextStream] listRoots response:', JSON.stringify(rootsResponse));
        if (rootsResponse?.roots) {
          this.ideRoots = rootsResponse.roots.map((r: { uri: string; name?: string }) => 
            r.uri.replace('file://', '')
          );
          console.error('[ContextStream] IDE roots detected via listRoots:', this.ideRoots);
        }
      } else {
        console.error('[ContextStream] Client does not support roots capability');
      }
    } catch (e) {
      console.error('[ContextStream] listRoots failed:', (e as Error)?.message || e);
    }
    
    // Method 2: Check environment variables that IDEs might set
    if (this.ideRoots.length === 0) {
      const envWorkspace = process.env.WORKSPACE_FOLDER 
        || process.env.VSCODE_WORKSPACE_FOLDER
        || process.env.PROJECT_DIR
        || process.env.PWD;
      
      if (envWorkspace && envWorkspace !== process.env.HOME) {
        console.error('[ContextStream] Using workspace from env:', envWorkspace);
        this.ideRoots = [envWorkspace];
      }
    }
    
    // Method 3: Use current working directory if it looks like a project
    if (this.ideRoots.length === 0) {
      const cwd = process.cwd();
      // Check if cwd contains common project indicators
      const fs = await import('fs');
      const projectIndicators = ['.git', 'package.json', 'Cargo.toml', 'pyproject.toml', '.contextstream'];
      const hasProjectIndicator = projectIndicators.some(f => {
        try {
          return fs.existsSync(`${cwd}/${f}`);
        } catch {
          return false;
        }
      });
      
      if (hasProjectIndicator) {
        console.error('[ContextStream] Using cwd as workspace:', cwd);
        this.ideRoots = [cwd];
      } else {
        console.error('[ContextStream] cwd does not look like a project:', cwd);
      }
    }

    // Use folder path hint if IDE roots not available
    if (this.ideRoots.length === 0 && this.folderPath) {
      this.ideRoots = [this.folderPath];
    }

    // Perform initialization
    this.initializationPromise = this._doInitialize();
    
    try {
      const result = await this.initializationPromise;
      return result as { contextSummary: string; context: Record<string, unknown> } | null;
    } finally {
      this.initializationPromise = null;
    }
  }

  private async _doInitialize(): Promise<{ contextSummary: string; context: Record<string, unknown> } | null> {
    try {
      console.error('[ContextStream] Auto-initializing session context...');
      console.error('[ContextStream] Using IDE roots:', this.ideRoots.length > 0 ? this.ideRoots : '(none - will use fallback)');
      
      const context = await this.client.initSession(
        {
          auto_index: true,
          include_recent_memory: true,
          include_decisions: true,
        },
        this.ideRoots
      ) as Record<string, unknown>;

      this.initialized = true;
      this.context = context;
      this.client.setDefaults({
        workspace_id: typeof context.workspace_id === 'string' ? (context.workspace_id as string) : undefined,
        project_id: typeof context.project_id === 'string' ? (context.project_id as string) : undefined,
      });

      console.error('[ContextStream] Workspace resolved:', context.workspace_name, '(source:', context.workspace_source, ')');

      // Build a concise summary for the AI
      const summary = this.buildContextSummary(context);
      
      console.error('[ContextStream] Auto-initialization complete');
      console.error(`[ContextStream] Workspace: ${context.workspace_name || 'unknown'}`);
      console.error(`[ContextStream] Project: ${context.project_id ? 'loaded' : 'none'}`);
      
      return { contextSummary: summary, context };
    } catch (error) {
      console.error('[ContextStream] Auto-initialization failed:', error);
      // Don't block the original tool call on init failure
      this.initialized = true; // Prevent retry loops
      return null;
    }
  }

  /**
   * Build a concise context summary for prepending to tool responses
   */
  private buildContextSummary(context: Record<string, unknown>): string {
    const parts: string[] = [];
    
    parts.push('═══════════════════════════════════════════');
    parts.push('🧠 AUTO-CONTEXT LOADED (ContextStream)');
    parts.push('═══════════════════════════════════════════');

    // Status
    if (context.status === 'requires_workspace_selection') {
      parts.push('');
      parts.push('⚠️  NEW FOLDER DETECTED');
      parts.push(`Folder: ${context.folder_name || 'unknown'}`);
      parts.push('');
      parts.push('Please ask the user which workspace this belongs to:');
      const candidates = context.workspace_candidates as Array<{ id: string; name: string; description?: string }> | undefined;
      if (candidates) {
        candidates.forEach((w, i) => {
          parts.push(`  ${i + 1}. ${w.name}${w.description ? ` - ${w.description}` : ''}`);
        });
      }
      parts.push('  • Or create a new workspace');
      parts.push('');
      parts.push('Use workspace_associate tool after user selects.');
      parts.push('═══════════════════════════════════════════');
      return parts.join('\n');
    }

    // Workspace info
    if (context.workspace_name) {
      parts.push(`📁 Workspace: ${context.workspace_name}`);
      // Debug: show how workspace was resolved
      if (context.workspace_source) {
        parts.push(`   (resolved via: ${context.workspace_source})`);
      }
      if (context.workspace_created) {
        parts.push('   (auto-created for this folder)');
      }
    }

    // Project info
    if (context.project_id) {
      const project = context.project as { name?: string } | undefined;
      parts.push(`📂 Project: ${project?.name || 'loaded'}`);
      if (context.project_created) {
        parts.push('   (auto-created, indexing in background)');
      }
      if (context.indexing_status === 'started') {
        parts.push('   ⏳ Code indexing in progress...');
      }
    }

    // Recent decisions
    const decisions = context.recent_decisions as { items?: Array<{ title?: string; content?: string }> } | undefined;
    if (decisions?.items && decisions.items.length > 0) {
      parts.push('');
      parts.push('📋 Recent Decisions:');
      decisions.items.slice(0, 3).forEach(d => {
        const title = d.title || d.content?.slice(0, 50) || 'Untitled';
        parts.push(`   • ${title}`);
      });
    }

    // Recent memory highlights
    const memory = context.recent_memory as { items?: Array<{ title?: string; event_type?: string }> } | undefined;
    if (memory?.items && memory.items.length > 0) {
      parts.push('');
      parts.push('🧠 Recent Context:');
      memory.items.slice(0, 3).forEach(m => {
        const title = m.title || 'Note';
        const type = m.event_type || '';
        parts.push(`   • [${type}] ${title}`);
      });
    }

    // IDE roots with detection method
    parts.push('');
    if (context.ide_roots && (context.ide_roots as string[]).length > 0) {
      const roots = context.ide_roots as string[];
      parts.push(`🖥️  IDE Roots: ${roots.join(', ')}`);
    } else {
      parts.push(`🖥️  IDE Roots: (none detected)`);
    }
    // Show detection method for debugging
    if (this.ideRoots.length > 0) {
      parts.push(`   Detection: ${this.ideRoots[0]}`);
    }

    parts.push('');
    parts.push('═══════════════════════════════════════════');
    parts.push('Use session_remember to save important context.');
    parts.push('Use session_recall to retrieve past context.');
    parts.push('═══════════════════════════════════════════');

    return parts.join('\n');
  }
}

/**
 * Type for wrapped tool handler
 */
type ToolHandler<T, R> = (input: T) => Promise<R>;

/**
 * Creates a wrapped tool handler that auto-initializes context on first call.
 * 
 * This is the key function that enables auto-context across all MCP clients.
 */
export function withAutoContext<T, R extends { content: Array<{ type: string; text: string }> }>(
  sessionManager: SessionManager,
  toolName: string,
  handler: ToolHandler<T, R>
): ToolHandler<T, R> {
  return async (input: T): Promise<R> => {
    // Skip auto-init for session_init itself (it handles its own initialization)
    const skipAutoInit = toolName === 'session_init';
    
    let contextPrefix = '';
    
    if (!skipAutoInit) {
      const autoInitResult = await sessionManager.autoInitialize();
      if (autoInitResult) {
        contextPrefix = autoInitResult.contextSummary + '\n\n';
      }
    }

    // Call the original handler
    const result = await handler(input);

    // Prepend context summary to the first text content (if we auto-initialized)
    if (contextPrefix && result.content && result.content.length > 0) {
      const firstContent = result.content[0];
      if (firstContent.type === 'text') {
        result.content[0] = {
          ...firstContent,
          text: contextPrefix + '--- Original Tool Response ---\n\n' + firstContent.text,
        };
      }
    }

    return result;
  };
}
