import { randomUUID } from "crypto";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { ContextStreamClient } from "./client.js";

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
  private sessionId: string;

  // Token tracking for context pressure calculation
  // Note: MCP servers cannot see actual token usage (AI responses, thinking, system prompts).
  // We use a heuristic: tracked tokens + (turns * estimated tokens per turn)
  private sessionTokens = 0;
  private contextThreshold = 70000; // Conservative default for 100k context window
  private conversationTurns = 0;
  // Each conversation turn typically includes: user message (~500), AI response (~1500),
  // system prompt overhead (~500), and reasoning (~1500). Conservative estimate: 3000/turn
  private static readonly TOKENS_PER_TURN_ESTIMATE = 3000;

  // Continuous checkpointing
  private toolCallCount = 0;
  private checkpointInterval = 20; // Save checkpoint every N tool calls
  private lastCheckpointAt = 0;
  private activeFiles: Set<string> = new Set();
  private recentToolCalls: Array<{ name: string; timestamp: number }> = [];
  private checkpointEnabled =
    process.env.CONTEXTSTREAM_CHECKPOINT_ENABLED?.toLowerCase() === "true";

  // Post-compaction restoration tracking
  // Tracks when context pressure was high/critical so we can detect post-compaction state
  private lastHighPressureAt: number | null = null;
  private lastHighPressureTokens = 0;
  private postCompactRestoreCompleted = false;

  constructor(
    private server: McpServer,
    private client: ContextStreamClient
  ) {
    // Generate a unique session ID for this MCP connection
    this.sessionId = `mcp-${randomUUID()}`;
  }

  /**
   * Get the unique session ID for this MCP connection.
   * Used for transcript saving and session association.
   */
  getSessionId(): string {
    return this.sessionId;
  }

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
   * Get the current folder path (if known)
   */
  getFolderPath(): string | null {
    return this.folderPath;
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
    const workspaceId =
      typeof context.workspace_id === "string" ? (context.workspace_id as string) : undefined;
    const projectId =
      typeof context.project_id === "string" ? (context.project_id as string) : undefined;
    if (workspaceId || projectId) {
      this.client.setDefaults({ workspace_id: workspaceId, project_id: projectId });
    }

    const contextFolderPath =
      typeof context.folder_path === "string" ? (context.folder_path as string) : undefined;
    if (contextFolderPath) {
      this.folderPath = contextFolderPath;
    }
  }

  /**
   * Update active workspace/project scope without resetting session identity.
   * Used when tools auto-resolve stale scope from folder/local index context.
   */
  updateScope(input: { workspace_id?: string; project_id?: string; folder_path?: string }) {
    const nextWorkspaceId =
      typeof input.workspace_id === "string" && input.workspace_id.trim()
        ? input.workspace_id
        : undefined;
    const nextProjectId =
      typeof input.project_id === "string" && input.project_id.trim()
        ? input.project_id
        : undefined;
    const nextFolderPath =
      typeof input.folder_path === "string" && input.folder_path.trim()
        ? input.folder_path
        : undefined;

    if (!this.context) {
      this.context = {};
    }

    if (nextWorkspaceId) {
      this.context.workspace_id = nextWorkspaceId;
    }
    if (nextProjectId) {
      this.context.project_id = nextProjectId;
    }
    if (nextFolderPath) {
      this.context.folder_path = nextFolderPath;
      this.folderPath = nextFolderPath;
    }

    if (nextWorkspaceId || nextProjectId) {
      this.initialized = true;
      this.client.setDefaults({
        workspace_id:
          typeof this.context.workspace_id === "string"
            ? (this.context.workspace_id as string)
            : undefined,
        project_id:
          typeof this.context.project_id === "string"
            ? (this.context.project_id as string)
            : undefined,
      });
    }
  }

  /**
   * Set the folder path hint (can be passed from tools that know the workspace path)
   */
  setFolderPath(path: string) {
    this.folderPath = path;
  }

  /**
   * Mark that context_smart has been called in this session.
   * Also increments the conversation turn counter for token estimation.
   */
  markContextSmartCalled() {
    this.contextSmartCalled = true;
    this.conversationTurns++;
  }

  /**
   * Get current session token count for context pressure calculation.
   *
   * This returns an ESTIMATED count based on:
   * 1. Tokens tracked through ContextStream tools (actual)
   * 2. Estimated tokens per conversation turn (heuristic)
   *
   * Note: MCP servers cannot see actual AI token usage (responses, thinking,
   * system prompts). This estimate helps provide a more realistic context
   * pressure signal.
   */
  getSessionTokens(): number {
    // Combine tracked tokens with turn-based estimation
    const turnEstimate = this.conversationTurns * SessionManager.TOKENS_PER_TURN_ESTIMATE;
    return this.sessionTokens + turnEstimate;
  }

  /**
   * Get the raw tracked tokens (without turn-based estimation).
   */
  getRawTrackedTokens(): number {
    return this.sessionTokens;
  }

  /**
   * Get the current conversation turn count.
   */
  getConversationTurns(): number {
    return this.conversationTurns;
  }

  /**
   * Get the context threshold (max tokens before compaction warning).
   */
  getContextThreshold(): number {
    return this.contextThreshold;
  }

  /**
   * Set a custom context threshold (useful if client provides model info).
   */
  setContextThreshold(threshold: number) {
    this.contextThreshold = threshold;
  }

  /**
   * Add tokens to the session count.
   * Call this after each tool response to track token accumulation.
   *
   * @param tokens - Exact token count or text to estimate
   */
  addTokens(tokens: number | string) {
    if (typeof tokens === "number") {
      this.sessionTokens += tokens;
    } else {
      // Estimate tokens from text (roughly 4 chars per token)
      this.sessionTokens += Math.ceil(tokens.length / 4);
    }
  }

  /**
   * Estimate tokens from a tool response.
   * Uses a simple heuristic: ~4 characters per token.
   */
  estimateTokens(content: string | object): number {
    const text = typeof content === "string" ? content : JSON.stringify(content);
    return Math.ceil(text.length / 4);
  }

  /**
   * Reset token count (e.g., after compaction or new session).
   */
  resetTokenCount() {
    this.sessionTokens = 0;
    this.conversationTurns = 0;
  }

  /**
   * Record that context pressure is high/critical.
   * Called when context_smart returns high or critical pressure level.
   */
  markHighContextPressure() {
    this.lastHighPressureAt = Date.now();
    this.lastHighPressureTokens = this.getSessionTokens();
  }

  /**
   * Check if we should attempt post-compaction restoration.
   *
   * Detection heuristic:
   * 1. We recorded high/critical context pressure recently (within 10 minutes)
   * 2. Current token count is very low (< 5000) compared to when pressure was high
   * 3. We haven't already restored in this session
   *
   * This indicates compaction likely happened and we should restore context.
   */
  shouldRestorePostCompact(): boolean {
    // Already restored
    if (this.postCompactRestoreCompleted) {
      return false;
    }

    // No high pressure recorded
    if (!this.lastHighPressureAt) {
      return false;
    }

    // High pressure was too long ago (> 10 minutes)
    const elapsed = Date.now() - this.lastHighPressureAt;
    if (elapsed > 10 * 60 * 1000) {
      return false;
    }

    // Current tokens should be significantly lower than when pressure was high
    // This indicates the context was compacted/reset
    const currentTokens = this.getSessionTokens();
    const tokenDrop = this.lastHighPressureTokens - currentTokens;

    // Require at least 50% drop and current tokens < 10k
    if (currentTokens > 10000 || tokenDrop < this.lastHighPressureTokens * 0.5) {
      return false;
    }

    return true;
  }

  /**
   * Mark post-compaction restoration as completed.
   * Prevents multiple restoration attempts in the same session.
   */
  markPostCompactRestoreCompleted() {
    this.postCompactRestoreCompleted = true;
    // Reset pressure tracking since we've restored
    this.lastHighPressureAt = null;
    this.lastHighPressureTokens = 0;
  }

  /**
   * Check if context_smart has been called and warn if not.
   * Returns true if a warning was shown, false otherwise.
   */
  warnIfContextSmartNotCalled(toolName: string): boolean {
    // Skip warning for these tools
    const skipWarningTools = [
      "session_init",
      "context_smart",
      "session_recall",
      "session_remember",
    ];
    if (skipWarningTools.includes(toolName)) {
      return false;
    }

    // Only warn once per session and only if session is initialized
    if (!this.initialized || this.contextSmartCalled || this.warningShown) {
      return false;
    }

    this.warningShown = true;
    console.warn(`[ContextStream] Warning: ${toolName} called without context_smart.`);
    console.warn(
      '[ContextStream] For best results, call context_smart(user_message="...") before other tools.'
    );
    console.warn(
      "[ContextStream] context_smart provides semantically relevant context for the user's query."
    );
    return true;
  }

  /**
   * Auto-initialize the session if not already done.
   * Returns context summary to prepend to tool response.
   *
   * This is the core of the auto-context feature.
   */
  async autoInitialize(): Promise<{
    contextSummary: string;
    context: Record<string, unknown>;
  } | null> {
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
      console.error("[ContextStream] Client capabilities:", JSON.stringify(capabilities));

      if (capabilities?.roots) {
        const rootsResponse = await this.server.server.listRoots();
        console.error("[ContextStream] listRoots response:", JSON.stringify(rootsResponse));
        if (rootsResponse?.roots) {
          this.ideRoots = rootsResponse.roots.map((r: { uri: string; name?: string }) =>
            r.uri.replace("file://", "")
          );
          console.error("[ContextStream] IDE roots detected via listRoots:", this.ideRoots);
        }
      } else {
        console.error("[ContextStream] Client does not support roots capability");
      }
    } catch (e) {
      console.error("[ContextStream] listRoots failed:", (e as Error)?.message || e);
    }

    // Method 2: Check environment variables that IDEs might set
    if (this.ideRoots.length === 0) {
      const envWorkspace =
        process.env.WORKSPACE_FOLDER ||
        process.env.VSCODE_WORKSPACE_FOLDER ||
        process.env.PROJECT_DIR ||
        process.env.PWD;

      if (envWorkspace && envWorkspace !== process.env.HOME) {
        console.error("[ContextStream] Using workspace from env:", envWorkspace);
        this.ideRoots = [envWorkspace];
      }
    }

    // Method 3: Use current working directory if it looks like a project
    if (this.ideRoots.length === 0) {
      const cwd = process.cwd();
      // Check if cwd contains common project indicators
      const fs = await import("fs");
      const projectIndicators = [
        ".git",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        ".contextstream",
      ];
      const hasProjectIndicator = projectIndicators.some((f) => {
        try {
          return fs.existsSync(`${cwd}/${f}`);
        } catch {
          return false;
        }
      });

      if (hasProjectIndicator) {
        console.error("[ContextStream] Using cwd as workspace:", cwd);
        this.ideRoots = [cwd];
      } else {
        console.error("[ContextStream] cwd does not look like a project:", cwd);
      }
    }

    // Use folder path hint if IDE roots not available
    if (this.ideRoots.length === 0 && this.folderPath) {
      this.ideRoots = [this.folderPath];
    }

    if (this.ideRoots.length > 0) {
      this.folderPath = this.ideRoots[0];
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

  private async _doInitialize(): Promise<{
    contextSummary: string;
    context: Record<string, unknown>;
  } | null> {
    try {
      console.error("[ContextStream] Auto-initializing session context...");
      console.error(
        "[ContextStream] Using IDE roots:",
        this.ideRoots.length > 0 ? this.ideRoots : "(none - will use fallback)"
      );

      const context = (await this.client.initSession(
        {
          auto_index: true,
          include_recent_memory: true,
          include_decisions: true,
        },
        this.ideRoots
      )) as Record<string, unknown>;

      this.initialized = true;
      this.context = context;
      this.client.setDefaults({
        workspace_id:
          typeof context.workspace_id === "string" ? (context.workspace_id as string) : undefined,
        project_id:
          typeof context.project_id === "string" ? (context.project_id as string) : undefined,
      });

      console.error(
        "[ContextStream] Workspace resolved:",
        context.workspace_name,
        "(source:",
        context.workspace_source,
        ")"
      );

      // Build a concise summary for the AI
      const summary = this.buildContextSummary(context);

      console.error("[ContextStream] Auto-initialization complete");
      console.error(`[ContextStream] Workspace: ${context.workspace_name || "unknown"}`);
      console.error(`[ContextStream] Project: ${context.project_id ? "loaded" : "none"}`);

      return { contextSummary: summary, context };
    } catch (error) {
      console.error("[ContextStream] Auto-initialization failed:", error);
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

    parts.push("═══════════════════════════════════════════");
    parts.push("⬡ ContextStream — Smart Context & Memory");
    parts.push("═══════════════════════════════════════════");

    // Status
    if (context.status === "requires_workspace_name") {
      parts.push("");
      parts.push("⚠️  NO WORKSPACE FOUND");
      parts.push(`Folder: ${context.folder_name || "unknown"}`);
      parts.push("");
      parts.push("Please ask the user for a name for the new workspace (recommended).");
      parts.push("Then create a project for this folder.");
      parts.push("");
      parts.push("Recommended: call `workspace_bootstrap` with:");
      if (typeof context.folder_path === "string") {
        parts.push(`  - folder_path: ${context.folder_path}`);
      } else {
        parts.push("  - folder_path: (your repo folder path)");
      }
      parts.push('  - workspace_name: "<user-provided name>"');
      parts.push("");
      parts.push("To continue without a workspace for now:");
      parts.push("  - call `session_init` again with `allow_no_workspace: true`");
      parts.push("");
      parts.push("═══════════════════════════════════════════");
      return parts.join("\n");
    }

    if (context.status === "requires_workspace_selection") {
      parts.push("");
      parts.push("⚠️  NEW FOLDER DETECTED");
      parts.push(`Folder: ${context.folder_name || "unknown"}`);
      parts.push("");
      parts.push("Please ask the user which workspace this belongs to:");
      const candidates = context.workspace_candidates as
        | Array<{ id: string; name: string; description?: string }>
        | undefined;
      if (candidates) {
        candidates.forEach((w, i) => {
          parts.push(`  ${i + 1}. ${w.name}${w.description ? ` - ${w.description}` : ""}`);
        });
      }
      parts.push("  • Or create a new workspace");
      parts.push("");
      parts.push("Use workspace_associate tool after user selects.");
      parts.push("");
      parts.push("To continue without a workspace for now:");
      parts.push("  - call `session_init` again with `allow_no_workspace: true`");
      parts.push("═══════════════════════════════════════════");
      return parts.join("\n");
    }

    // Workspace info
    if (context.workspace_name) {
      parts.push(`📁 Workspace: ${context.workspace_name}`);
      // Debug: show how workspace was resolved
      if (context.workspace_source) {
        parts.push(`   (resolved via: ${context.workspace_source})`);
      }
      if (context.workspace_created) {
        parts.push("   (auto-created for this folder)");
      }
    }

    // Project info
    if (context.project_id) {
      const project = context.project as { name?: string } | undefined;
      parts.push(`📂 Project: ${project?.name || "loaded"}`);
      if (context.project_created) {
        parts.push("   (auto-created, indexing in background)");
      }
      if (context.indexing_status === "started") {
        parts.push("   ⏳ Code indexing in progress...");
      }
    }

    // Recent decisions
    const decisions = context.recent_decisions as
      | { items?: Array<{ title?: string; content?: string }> }
      | undefined;
    if (decisions?.items && decisions.items.length > 0) {
      parts.push("");
      parts.push("📋 Recent Decisions:");
      decisions.items.slice(0, 3).forEach((d) => {
        const title = d.title || d.content?.slice(0, 50) || "Untitled";
        parts.push(`   • ${title}`);
      });
    }

    // Recent memory highlights
    const memory = context.recent_memory as
      | { items?: Array<{ title?: string; event_type?: string }> }
      | undefined;
    if (memory?.items && memory.items.length > 0) {
      parts.push("");
      parts.push("📋 Recent Context:");
      memory.items.slice(0, 3).forEach((m) => {
        const title = m.title || "Note";
        const type = m.event_type || "";
        parts.push(`   • [${type}] ${title}`);
      });
    }

    // High-priority lessons (warnings from past mistakes)
    const lessonsWarning =
      typeof context.lessons_warning === "string" ? (context.lessons_warning as string) : undefined;
    const lessons = Array.isArray(context.lessons)
      ? (context.lessons as Array<{ title?: string; severity?: string }>)
      : [];
    if (lessonsWarning || lessons.length > 0) {
      parts.push("");
      parts.push("⚠️  Lessons (review before changes):");
      if (lessonsWarning) {
        parts.push(`   ${lessonsWarning}`);
      }
      lessons.slice(0, 3).forEach((l) => {
        const title = l.title || "Lesson";
        const severity = l.severity || "unknown";
        parts.push(`   • [${severity}] ${title}`);
      });
      parts.push('   Use session_get_lessons(query="...") for details.');
    }

    // IDE roots with detection method
    parts.push("");
    if (context.ide_roots && (context.ide_roots as string[]).length > 0) {
      const roots = context.ide_roots as string[];
      parts.push(`🖥️  IDE Roots: ${roots.join(", ")}`);
    } else {
      parts.push(`🖥️  IDE Roots: (none detected)`);
    }
    // Show detection method for debugging
    if (this.ideRoots.length > 0) {
      parts.push(`   Detection: ${this.ideRoots[0]}`);
    }

    parts.push("");
    parts.push("═══════════════════════════════════════════");
    parts.push("Use session_remember to save important context.");
    parts.push("Use session_recall to retrieve past context.");
    parts.push("═══════════════════════════════════════════");

    return parts.join("\n");
  }

  // =========================================================================
  // Continuous Checkpointing
  // =========================================================================

  /**
   * Track a tool call for checkpointing purposes.
   * Call this after each tool execution to track files and trigger periodic checkpoints.
   */
  trackToolCall(toolName: string, input?: Record<string, unknown>): void {
    this.toolCallCount++;
    this.recentToolCalls.push({ name: toolName, timestamp: Date.now() });

    // Keep only last 50 tool calls
    if (this.recentToolCalls.length > 50) {
      this.recentToolCalls = this.recentToolCalls.slice(-50);
    }

    // Track files from common file operations
    if (input) {
      const filePath =
        (input.file_path as string) || (input.notebook_path as string) || (input.path as string);
      if (filePath && typeof filePath === "string") {
        this.activeFiles.add(filePath);
        // Keep only last 30 files
        if (this.activeFiles.size > 30) {
          const arr = Array.from(this.activeFiles);
          this.activeFiles = new Set(arr.slice(-30));
        }
      }
    }

    // Check if we should save a checkpoint
    this.maybeCheckpoint();
  }

  /**
   * Save a checkpoint if the interval has been reached.
   */
  private async maybeCheckpoint(): Promise<void> {
    if (!this.checkpointEnabled || !this.initialized || !this.context) {
      return;
    }

    const callsSinceLastCheckpoint = this.toolCallCount - this.lastCheckpointAt;
    if (callsSinceLastCheckpoint < this.checkpointInterval) {
      return;
    }

    this.lastCheckpointAt = this.toolCallCount;
    await this.saveCheckpoint("periodic");
  }

  /**
   * Get the list of active files being worked on.
   */
  getActiveFiles(): string[] {
    return Array.from(this.activeFiles);
  }

  /**
   * Get recent tool call names.
   */
  getRecentToolNames(): string[] {
    return this.recentToolCalls.map((t) => t.name);
  }

  /**
   * Get the current tool call count.
   */
  getToolCallCount(): number {
    return this.toolCallCount;
  }

  /**
   * Save a checkpoint snapshot to ContextStream.
   */
  async saveCheckpoint(trigger: "periodic" | "milestone" | "manual"): Promise<boolean> {
    if (!this.initialized || !this.context) {
      return false;
    }

    const workspaceId = this.context.workspace_id as string | undefined;
    if (!workspaceId) {
      return false;
    }

    const checkpointData = {
      trigger,
      checkpoint_number: Math.floor(this.toolCallCount / this.checkpointInterval),
      tool_call_count: this.toolCallCount,
      session_tokens: this.sessionTokens,
      active_files: this.getActiveFiles(),
      recent_tools: this.getRecentToolNames().slice(-10),
      captured_at: new Date().toISOString(),
      auto_captured: true,
    };

    try {
      await this.client.captureContext({
        workspace_id: workspaceId,
        project_id: this.context.project_id as string | undefined,
        event_type: "session_snapshot",
        title: `Checkpoint #${checkpointData.checkpoint_number} (${trigger})`,
        content: JSON.stringify(checkpointData),
        importance: trigger === "periodic" ? "low" : "medium",
        tags: ["session_snapshot", "checkpoint", trigger],
      });
      return true;
    } catch (err) {
      console.error("[ContextStream] Failed to save checkpoint:", err);
      return false;
    }
  }

  /**
   * Enable or disable continuous checkpointing.
   */
  setCheckpointEnabled(enabled: boolean): void {
    this.checkpointEnabled = enabled;
  }

  /**
   * Set the checkpoint interval (tool calls between checkpoints).
   */
  setCheckpointInterval(interval: number): void {
    this.checkpointInterval = Math.max(5, interval); // Minimum 5 to avoid spam
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
    const skipAutoInit = toolName === "session_init";

    let contextPrefix = "";

    if (!skipAutoInit) {
      const autoInitResult = await sessionManager.autoInitialize();
      if (autoInitResult) {
        contextPrefix = autoInitResult.contextSummary + "\n\n";
      }
    }

    // Call the original handler
    const result = await handler(input);

    // Track the tool call for continuous checkpointing
    sessionManager.trackToolCall(toolName, input as Record<string, unknown>);

    // Prepend context summary to the first text content (if we auto-initialized)
    if (contextPrefix && result.content && result.content.length > 0) {
      const firstContent = result.content[0];
      if (firstContent.type === "text") {
        result.content[0] = {
          ...firstContent,
          text: contextPrefix + "--- Original Tool Response ---\n\n" + firstContent.text,
        };
      }
    }

    return result;
  };
}
