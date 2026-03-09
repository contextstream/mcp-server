/**
 * No-op hook handler for compatibility hook names that do not need local behavior
 * in the TypeScript server. Exits successfully without emitting output.
 */
export async function runNoopHook(): Promise<void> {
  return;
}
