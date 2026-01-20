export type TokenSavingsToolType =
  | "context_smart"
  | "ai_context_budget"
  | "search_semantic"
  | "search_hybrid"
  | "search_keyword"
  | "search_pattern"
  | "search_exhaustive"
  | "search_refactor"
  | "session_recall"
  | "session_smart_search"
  | "session_user_context"
  | "session_summary"
  | "graph_dependencies"
  | "graph_impact"
  | "graph_call_path"
  | "graph_related"
  | "memory_search"
  | "memory_decisions"
  | "memory_timeline"
  | "memory_summary";

type TrackTokenSavingsPayload = {
  tool: string;
  workspace_id?: string;
  project_id?: string;
  candidate_chars: number;
  context_chars: number;
  max_tokens?: number;
  metadata?: unknown;
};

type TokenSavingsClient = {
  trackTokenSavings: (body: TrackTokenSavingsPayload) => Promise<unknown>;
};

const TOKEN_SAVINGS_FORMULA_VERSION = 1;
const MAX_CHARS_PER_EVENT = 20_000_000; // Must stay aligned with API-side guardrails
const BASE_OVERHEAD_CHARS = 500;

// Multipliers to estimate what candidate_chars would be without compression.
// These represent typical expansion from compressed context to full file reads.
export const CANDIDATE_MULTIPLIERS: Record<TokenSavingsToolType, number> = {
  // context_smart: Replaces reading multiple files to gather context
  context_smart: 5.0,
  ai_context_budget: 5.0,

  // search: Semantic search replaces iterative Glob/Grep/Read cycles
  search_semantic: 4.5,
  search_hybrid: 4.0,
  search_keyword: 2.5,
  search_pattern: 3.0,
  search_exhaustive: 3.5,
  search_refactor: 3.0,

  // session: Recall/search replaces reading through history
  session_recall: 5.0,
  session_smart_search: 4.0,
  session_user_context: 3.0,
  session_summary: 4.0,

  // graph: Would require extensive file traversal
  graph_dependencies: 8.0,
  graph_impact: 10.0,
  graph_call_path: 8.0,
  graph_related: 6.0,

  // memory: Context retrieval
  memory_search: 3.5,
  memory_decisions: 3.0,
  memory_timeline: 3.0,
  memory_summary: 4.0,
};

function clampCharCount(value: number): number {
  if (!Number.isFinite(value) || value <= 0) return 0;
  return Math.min(MAX_CHARS_PER_EVENT, Math.floor(value));
}

/**
 * Track token savings for a tool call (fire-and-forget).
 * This enables the Token Savings dashboard features.
 *
 * candidate_chars is an estimated baseline (what a "default tools" workflow would need).
 * context_chars is what we actually returned to the model/tool consumer.
 */
export function trackToolTokenSavings(
  client: TokenSavingsClient,
  tool: TokenSavingsToolType,
  contextText: string,
  params?: { workspace_id?: string; project_id?: string; max_tokens?: number },
  extraMetadata?: Record<string, unknown>
): void {
  try {
    const contextChars = clampCharCount(contextText.length);
    const multiplier = CANDIDATE_MULTIPLIERS[tool] ?? 3.0;
    const baseOverhead = contextChars > 0 ? BASE_OVERHEAD_CHARS : 0;
    const estimatedCandidate = Math.round(contextChars * multiplier + baseOverhead);
    const candidateChars = Math.max(contextChars, clampCharCount(estimatedCandidate));

    client
      .trackTokenSavings({
        tool,
        workspace_id: params?.workspace_id,
        project_id: params?.project_id,
        candidate_chars: candidateChars,
        context_chars: contextChars,
        max_tokens: params?.max_tokens,
        metadata: {
          method: "multiplier_estimate",
          formula_version: TOKEN_SAVINGS_FORMULA_VERSION,
          source: "mcp-server",
          multiplier,
          base_overhead_chars: baseOverhead,
          ...(extraMetadata ?? {}),
        },
      })
      .catch(() => {
        // Silently ignore tracking errors - this is best-effort analytics
      });
  } catch {
    // Silently ignore any errors in tracking setup
  }
}

