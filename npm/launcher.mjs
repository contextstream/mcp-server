import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { createReadStream } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  rename,
  stat,
  unlink,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const packagePath = fileURLToPath(new URL("../package.json", import.meta.url));
const packageMetadata = JSON.parse(await readFile(packagePath, "utf8"));

export const PACKAGE_VERSION = packageMetadata.version;
export const RELEASE_ROOT =
  "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/";

const MAX_DOWNLOAD_BYTES = 200 * 1024 * 1024;
const FETCH_TIMEOUT_MS = 30_000;
const LOCK_TIMEOUT_MS = 30_000;
const STALE_LOCK_MS = 120_000;

const PLATFORM_ARTIFACTS = Object.freeze({
  "linux-x64": "contextstream-mcp-linux-x64",
  "linux-arm64": "contextstream-mcp-linux-arm64",
  "darwin-x64": "contextstream-mcp-darwin-x64",
  "darwin-arm64": "contextstream-mcp-darwin-arm64",
  "win32-x64": "contextstream-mcp-win-x64.exe",
});

class FetchFailure extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "FetchFailure";
  }
}

export function artifactFor(platform = process.platform, arch = process.arch) {
  const key = `${platform}-${arch}`;
  const artifact = PLATFORM_ARTIFACTS[key];
  if (!artifact) {
    throw new Error(
      `@contextstream/mcp-server ${PACKAGE_VERSION} does not publish a binary for ${key}`,
    );
  }
  const manifestKey = platform === "win32" ? "win-x64" : key;
  return { artifact, key, manifestKey };
}

function truthy(value) {
  return /^(1|true|yes|on|enabled)$/i.test(value ?? "");
}

function releasePrefix(version, env) {
  const override = env.CONTEXTSTREAM_MCP_RELEASE_BASE_URL;
  const testOverrideEnabled = truthy(env.CONTEXTSTREAM_MCP_TEST_ALLOW_HTTP);
  if (override && !testOverrideEnabled) {
    throw new Error(
      "release base URL overrides are disabled outside the explicit test harness",
    );
  }
  const prefix = override
    ? new URL(override.endsWith("/") ? override : `${override}/`)
    : new URL(`mcp/v${version}/`, RELEASE_ROOT);
  if (
    prefix.protocol !== "https:" &&
    !(testOverrideEnabled && prefix.protocol === "http:")
  ) {
    throw new Error("release base URL must use HTTPS");
  }
  if (prefix.username || prefix.password || prefix.search || prefix.hash) {
    throw new Error("release base URL must not contain credentials, a query, or a fragment");
  }
  return prefix;
}

function defaultCacheRoot(env) {
  if (env.CONTEXTSTREAM_MCP_CACHE_DIR) {
    return path.resolve(env.CONTEXTSTREAM_MCP_CACHE_DIR);
  }
  if (process.platform === "win32" && env.LOCALAPPDATA) {
    return path.join(env.LOCALAPPDATA, "ContextStream", "mcp");
  }
  if (env.XDG_CACHE_HOME) {
    return path.join(env.XDG_CACHE_HOME, "contextstream", "mcp");
  }
  return path.join(homedir(), ".cache", "contextstream", "mcp");
}

async function fetchBuffer(url, maxBytes = MAX_DOWNLOAD_BYTES) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
  try {
    let response;
    try {
      response = await fetch(url, {
        redirect: "error",
        signal: controller.signal,
        headers: { "user-agent": `contextstream-npm-launcher/${PACKAGE_VERSION}` },
      });
    } catch (error) {
      throw new FetchFailure(`could not download ${url}`, { cause: error });
    }
    if (!response.ok) {
      throw new FetchFailure(`download returned HTTP ${response.status} for ${url}`);
    }
    const declaredLength = Number(response.headers.get("content-length"));
    if (Number.isFinite(declaredLength) && declaredLength > maxBytes) {
      throw new Error(`release object exceeds the ${maxBytes}-byte safety limit`);
    }
    if (!response.body) {
      throw new FetchFailure(`download returned no body for ${url}`);
    }

    const chunks = [];
    let total = 0;
    for await (const chunk of response.body) {
      const buffer = Buffer.from(chunk);
      total += buffer.length;
      if (total > maxBytes) {
        throw new Error(`release object exceeds the ${maxBytes}-byte safety limit`);
      }
      chunks.push(buffer);
    }
    return Buffer.concat(chunks, total);
  } finally {
    clearTimeout(timeout);
  }
}

function parseChecksums(contents) {
  const checksums = new Map();
  for (const line of contents.trim().split(/\r?\n/u)) {
    const match = /^([0-9a-f]{64})  ([A-Za-z0-9._-]+)$/u.exec(line);
    if (!match || checksums.has(match[2])) {
      throw new Error("release checksums.txt has an invalid or duplicate entry");
    }
    checksums.set(match[2], match[1]);
  }
  return checksums;
}

async function releaseMetadata({ artifact, manifestKey, prefix, version }) {
  const manifestUrl = new URL("version.json", prefix);
  const checksumsUrl = new URL("checksums.txt", prefix);
  const [manifestBytes, checksumBytes] = await Promise.all([
    fetchBuffer(manifestUrl, 128 * 1024),
    fetchBuffer(checksumsUrl, 128 * 1024),
  ]);

  let manifest;
  try {
    manifest = JSON.parse(manifestBytes.toString("utf8"));
  } catch (error) {
    throw new Error("release version.json is not valid JSON", { cause: error });
  }
  if (
    !manifest ||
    typeof manifest !== "object" ||
    manifest.version !== version ||
    !manifest.files ||
    manifest.files[manifestKey] !== artifact
  ) {
    throw new Error("release manifest does not match this exact npm package/platform");
  }

  const checksums = parseChecksums(checksumBytes.toString("utf8"));
  const sha256 = checksums.get(artifact);
  if (!sha256) {
    throw new Error(`release checksums do not declare ${artifact}`);
  }
  return {
    artifact,
    manifestKey,
    prefix: prefix.href,
    sha256,
    version,
  };
}

async function sha256File(filename) {
  const hash = createHash("sha256");
  await new Promise((resolve, reject) => {
    const stream = createReadStream(filename);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("error", reject);
    stream.on("end", resolve);
  });
  return hash.digest("hex");
}

async function regularFile(filename) {
  try {
    const details = await lstat(filename);
    return details.isFile() && !details.isSymbolicLink();
  } catch {
    return false;
  }
}

async function validCachedBinary(binaryPath, metadataPath, expected = undefined) {
  if (!(await regularFile(binaryPath)) || !(await regularFile(metadataPath))) {
    return false;
  }
  let metadata;
  try {
    metadata = JSON.parse(await readFile(metadataPath, "utf8"));
  } catch {
    return false;
  }
  if (
    metadata.version !== PACKAGE_VERSION ||
    !/^[0-9a-f]{64}$/u.test(metadata.sha256 ?? "") ||
    (expected &&
      (metadata.sha256 !== expected.sha256 ||
        metadata.artifact !== expected.artifact ||
        metadata.manifestKey !== expected.manifestKey))
  ) {
    return false;
  }
  return (await sha256File(binaryPath)) === metadata.sha256;
}

async function acquireLock(lockPath, ready) {
  const deadline = Date.now() + LOCK_TIMEOUT_MS;
  while (Date.now() < deadline) {
    try {
      const handle = await open(lockPath, "wx", 0o600);
      await handle.writeFile(`${process.pid} ${Date.now()}\n`);
      return async () => {
        await handle.close().catch(() => undefined);
        await unlink(lockPath).catch(() => undefined);
      };
    } catch (error) {
      if (error?.code !== "EEXIST") throw error;
      if (await ready()) return null;
      try {
        const details = await stat(lockPath);
        if (Date.now() - details.mtimeMs > STALE_LOCK_MS) {
          await unlink(lockPath);
          continue;
        }
      } catch (statError) {
        if (statError?.code !== "ENOENT") throw statError;
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  throw new Error("timed out waiting for another launcher to populate the cache");
}

async function installCachedBinary(binaryPath, metadataPath, metadata) {
  const unique = `${process.pid}-${randomUUID()}`;
  const temporaryBinary = `${binaryPath}.${unique}.tmp`;
  const temporaryMetadata = `${metadataPath}.${unique}.tmp`;
  try {
    const bytes = await fetchBuffer(new URL(metadata.artifact, metadata.prefix));
    const digest = createHash("sha256").update(bytes).digest("hex");
    if (digest !== metadata.sha256) {
      throw new Error(
        `downloaded ${metadata.artifact} failed SHA-256 verification`,
      );
    }
    await writeFile(temporaryBinary, bytes, { flag: "wx", mode: 0o700 });
    if (process.platform !== "win32") await chmod(temporaryBinary, 0o700);
    await writeFile(
      temporaryMetadata,
      `${JSON.stringify({ ...metadata, installedAt: new Date().toISOString() }, null, 2)}\n`,
      { flag: "wx", mode: 0o600 },
    );
    await rename(temporaryBinary, binaryPath);
    await rename(temporaryMetadata, metadataPath);
  } finally {
    await unlink(temporaryBinary).catch(() => undefined);
    await unlink(temporaryMetadata).catch(() => undefined);
  }
}

export async function ensureBinary({
  env = process.env,
  platform = process.platform,
  arch = process.arch,
} = {}) {
  const selected = artifactFor(platform, arch);
  const cacheDirectory = path.join(
    defaultCacheRoot(env),
    `v${PACKAGE_VERSION}`,
    selected.key,
  );
  const binaryPath = path.join(
    cacheDirectory,
    platform === "win32" ? "contextstream-mcp.exe" : "contextstream-mcp",
  );
  const metadataPath = path.join(cacheDirectory, "metadata.json");
  const lockPath = path.join(cacheDirectory, ".install.lock");
  await mkdir(cacheDirectory, { recursive: true, mode: 0o700 });

  if (truthy(env.CONTEXTSTREAM_MCP_OFFLINE)) {
    if (await validCachedBinary(binaryPath, metadataPath)) return binaryPath;
    throw new Error(
      `offline mode requested, but no verified ${PACKAGE_VERSION} binary is cached`,
    );
  }

  let metadata;
  try {
    metadata = await releaseMetadata({
      ...selected,
      prefix: releasePrefix(PACKAGE_VERSION, env),
      version: PACKAGE_VERSION,
    });
  } catch (error) {
    if (error instanceof FetchFailure && (await validCachedBinary(binaryPath, metadataPath))) {
      process.stderr.write(
        `ContextStream launcher: release service unavailable; using verified cached v${PACKAGE_VERSION}.\n`,
      );
      return binaryPath;
    }
    throw error;
  }

  if (await validCachedBinary(binaryPath, metadataPath, metadata)) return binaryPath;
  const release = await acquireLock(lockPath, () =>
    validCachedBinary(binaryPath, metadataPath, metadata),
  );
  if (release === null) return binaryPath;
  try {
    if (!(await validCachedBinary(binaryPath, metadataPath, metadata))) {
      await installCachedBinary(binaryPath, metadataPath, metadata);
    }
  } finally {
    await release();
  }
  if (!(await validCachedBinary(binaryPath, metadataPath, metadata))) {
    throw new Error("cached ContextStream binary failed post-install verification");
  }
  return binaryPath;
}

function forwardSignals(child) {
  const signals = process.platform === "win32" ? ["SIGINT", "SIGTERM"] : ["SIGINT", "SIGTERM", "SIGHUP"];
  const handlers = new Map();
  for (const signal of signals) {
    const handler = () => child.kill(signal);
    handlers.set(signal, handler);
    process.on(signal, handler);
  }
  return () => {
    for (const [signal, handler] of handlers) process.off(signal, handler);
  };
}

export async function run({ mode = "server", args = process.argv.slice(2), env = process.env } = {}) {
  if (!matchesMode(mode)) throw new Error(`unsupported launcher mode: ${mode}`);
  const binaryPath = await ensureBinary({ env });
  const childArgs = mode === "hook" ? ["hook", ...args] : args;
  const child = spawn(binaryPath, childArgs, {
    env: {
      ...env,
      CONTEXTSTREAM_DISABLE_SELF_UPDATE: "1",
      CONTEXTSTREAM_MANAGED_BY_NPM: "1",
      CONTEXTSTREAM_NPM_PACKAGE_VERSION: PACKAGE_VERSION,
    },
    stdio: "inherit",
    windowsHide: true,
  });
  const stopForwarding = forwardSignals(child);
  const result = await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code, signal }));
  }).finally(stopForwarding);

  if (result.signal) {
    process.kill(process.pid, result.signal);
    return;
  }
  process.exitCode = result.code ?? 1;
}

function matchesMode(mode) {
  return mode === "server" || mode === "hook";
}
