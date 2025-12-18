/**
 * Simple in-memory cache with TTL support for MCP client.
 * 
 * This reduces HTTP roundtrips for frequently accessed data like:
 * - Workspace info (rarely changes)
 * - Project info (rarely changes)
 * - Recent memory (can be cached for 30-60 seconds)
 */

interface CacheEntry<T> {
  value: T;
  expiresAt: number;
}

export class MemoryCache {
  private cache = new Map<string, CacheEntry<unknown>>();
  private cleanupInterval: NodeJS.Timeout | null = null;

  constructor(cleanupIntervalMs = 60_000) {
    // Periodic cleanup of expired entries
    this.cleanupInterval = setInterval(() => this.cleanup(), cleanupIntervalMs);
    // Don't keep the process alive for one-shot commands like `--version`
    this.cleanupInterval.unref?.();
  }

  /**
   * Get a cached value if it exists and hasn't expired
   */
  get<T>(key: string): T | undefined {
    const entry = this.cache.get(key) as CacheEntry<T> | undefined;
    if (!entry) return undefined;
    
    if (Date.now() > entry.expiresAt) {
      this.cache.delete(key);
      return undefined;
    }
    
    return entry.value;
  }

  /**
   * Set a value with TTL in milliseconds
   */
  set<T>(key: string, value: T, ttlMs: number): void {
    this.cache.set(key, {
      value,
      expiresAt: Date.now() + ttlMs,
    });
  }

  /**
   * Delete a specific key
   */
  delete(key: string): boolean {
    return this.cache.delete(key);
  }

  /**
   * Delete all keys matching a prefix
   */
  deleteByPrefix(prefix: string): number {
    let deleted = 0;
    for (const key of this.cache.keys()) {
      if (key.startsWith(prefix)) {
        this.cache.delete(key);
        deleted++;
      }
    }
    return deleted;
  }

  /**
   * Clear all cached entries
   */
  clear(): void {
    this.cache.clear();
  }

  /**
   * Remove expired entries
   */
  private cleanup(): void {
    const now = Date.now();
    for (const [key, entry] of this.cache.entries()) {
      if (now > entry.expiresAt) {
        this.cache.delete(key);
      }
    }
  }

  /**
   * Stop the cleanup interval
   */
  destroy(): void {
    if (this.cleanupInterval) {
      clearInterval(this.cleanupInterval);
      this.cleanupInterval = null;
    }
    this.cache.clear();
  }
}

// Default TTLs for different data types (in milliseconds)
export const CacheTTL = {
  // Workspace info rarely changes - cache for 5 minutes
  WORKSPACE: 5 * 60 * 1000,
  
  // Project info rarely changes - cache for 5 minutes
  PROJECT: 5 * 60 * 1000,
  
  // Session init context - cache for 60 seconds
  SESSION_INIT: 60 * 1000,
  
  // Memory events - cache for 30 seconds (they change more often)
  MEMORY_EVENTS: 30 * 1000,
  
  // Search results - cache for 60 seconds
  SEARCH: 60 * 1000,
  
  // User preferences - cache for 5 minutes
  USER_PREFS: 5 * 60 * 1000,

  // Credits/plan - cache briefly to reflect upgrades quickly
  CREDIT_BALANCE: 60 * 1000,
} as const;

// Cache key builders
export const CacheKeys = {
  workspace: (id: string) => `workspace:${id}`,
  workspaceList: (userId: string) => `workspaces:${userId}`,
  project: (id: string) => `project:${id}`,
  projectList: (workspaceId: string) => `projects:${workspaceId}`,
  sessionInit: (workspaceId?: string, projectId?: string) => 
    `session_init:${workspaceId || ''}:${projectId || ''}`,
  memoryEvents: (workspaceId: string) => `memory:${workspaceId}`,
  search: (query: string, workspaceId?: string) => 
    `search:${workspaceId || ''}:${query}`,
  creditBalance: () => 'credits:balance',
} as const;

// Global cache instance
export const globalCache = new MemoryCache();
