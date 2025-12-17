import { createRequire } from 'module';

export function getVersion(): string {
  try {
    const require = createRequire(import.meta.url);
    const pkg = require('../package.json') as { version?: string } | undefined;
    const version = pkg?.version;
    if (typeof version === 'string' && version.trim()) return version.trim();
  } catch {
    // ignore
  }
  return 'unknown';
}

export const VERSION = getVersion();
