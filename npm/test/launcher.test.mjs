import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { chmod, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import http from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { artifactFor, PACKAGE_VERSION } from "../launcher.mjs";

const execFileAsync = promisify(execFile);
const repositoryRoot = fileURLToPath(new URL("../..", import.meta.url));
const bins = {
  hook: path.join(repositoryRoot, "npm", "bin", "contextstream-hook.mjs"),
  mcp: path.join(repositoryRoot, "npm", "bin", "mcp-server.mjs"),
  rustAlias: path.join(repositoryRoot, "npm", "bin", "contextstream-mcp.mjs"),
};

const integrationOptions = {
  skip: process.platform === "win32" ? "fixture uses a POSIX shebang" : false,
};

async function temporaryDirectory(t, label) {
  const directory = await mkdtemp(path.join(tmpdir(), `${label}-`));
  t.after(() => rm(directory, { force: true, recursive: true }));
  return directory;
}

function executableFixture(label = "fixture") {
  return Buffer.from(`#!/usr/bin/env node
console.log(JSON.stringify({
  label: ${JSON.stringify(label)},
  args: process.argv.slice(2),
  managed: process.env.CONTEXTSTREAM_MANAGED_BY_NPM,
  selfUpdateDisabled: process.env.CONTEXTSTREAM_DISABLE_SELF_UPDATE,
  packageVersion: process.env.CONTEXTSTREAM_NPM_PACKAGE_VERSION
}));
`);
}

async function releaseServer(body, { badChecksum = false, delayArtifactMs = 0 } = {}) {
  const selected = artifactFor();
  const digest = createHash("sha256").update(body).digest("hex");
  const requests = { artifact: 0, checksums: 0, manifest: 0 };
  const manifest = JSON.stringify({
    version: PACKAGE_VERSION,
    files: {
      "linux-x64": "contextstream-mcp-linux-x64",
      "linux-arm64": "contextstream-mcp-linux-arm64",
      "darwin-x64": "contextstream-mcp-darwin-x64",
      "darwin-arm64": "contextstream-mcp-darwin-arm64",
      "win-x64": "contextstream-mcp-win-x64.exe",
    },
  });
  const checksums = `${badChecksum ? "0".repeat(64) : digest}  ${selected.artifact}\n`;
  const basePath = `/mcp/v${PACKAGE_VERSION}/`;

  const server = http.createServer(async (request, response) => {
    const pathname = new URL(request.url, "http://localhost").pathname;
    if (pathname === `${basePath}version.json`) {
      requests.manifest += 1;
      response.end(manifest);
      return;
    }
    if (pathname === `${basePath}checksums.txt`) {
      requests.checksums += 1;
      response.end(checksums);
      return;
    }
    if (pathname === `${basePath}${selected.artifact}`) {
      requests.artifact += 1;
      if (delayArtifactMs) {
        await new Promise((resolve) => setTimeout(resolve, delayArtifactMs));
      }
      response.end(body);
      return;
    }
    response.writeHead(404).end("missing");
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  return {
    close: () => new Promise((resolve) => server.close(resolve)),
    prefix: `http://127.0.0.1:${address.port}${basePath}`,
    requests,
  };
}

function launcherEnvironment(cacheDirectory, prefix, extra = {}) {
  return {
    ...process.env,
    CONTEXTSTREAM_MCP_CACHE_DIR: cacheDirectory,
    CONTEXTSTREAM_MCP_RELEASE_BASE_URL: prefix,
    CONTEXTSTREAM_MCP_TEST_ALLOW_HTTP: "1",
    ...extra,
  };
}

async function launch(bin, args, env) {
  const result = await execFileAsync(process.execPath, [bin, ...args], {
    env,
    timeout: 20_000,
  });
  return JSON.parse(result.stdout.trim().split(/\r?\n/u).at(-1));
}

test("platform map covers every published npm compatibility target", () => {
  assert.deepEqual(artifactFor("linux", "x64"), {
    artifact: "contextstream-mcp-linux-x64",
    key: "linux-x64",
    manifestKey: "linux-x64",
  });
  assert.equal(artifactFor("linux", "arm64").manifestKey, "linux-arm64");
  assert.equal(artifactFor("darwin", "x64").manifestKey, "darwin-x64");
  assert.equal(artifactFor("darwin", "arm64").manifestKey, "darwin-arm64");
  assert.equal(artifactFor("win32", "x64").manifestKey, "win-x64");
  assert.throws(() => artifactFor("win32", "arm64"), /does not publish/u);
});

test("release mirror override requires the explicit test gate", integrationOptions, async (t) => {
  const cache = await temporaryDirectory(t, "contextstream-override-gate-cache");
  const env = {
    ...process.env,
    CONTEXTSTREAM_MCP_CACHE_DIR: cache,
    CONTEXTSTREAM_MCP_RELEASE_BASE_URL: "https://example.invalid/mcp/v1.0.0/",
  };

  await assert.rejects(
    () => execFileAsync(process.execPath, [bins.mcp, "doctor"], { env }),
    (error) => {
      assert.match(error.stderr, /overrides are disabled/u);
      return true;
    },
  );
});

test("all three aliases invoke the exact binary and hook adds its subcommand", integrationOptions, async (t) => {
  const cache = await temporaryDirectory(t, "contextstream-alias-cache");
  const release = await releaseServer(executableFixture("aliases"));
  t.after(release.close);
  const env = launcherEnvironment(cache, release.prefix);

  const mcp = await launch(bins.mcp, ["doctor"], env);
  const rustAlias = await launch(bins.rustAlias, ["setup", "--yes"], env);
  const hook = await launch(bins.hook, ["pre-tool-use"], env);

  assert.deepEqual(mcp.args, ["doctor"]);
  assert.deepEqual(rustAlias.args, ["setup", "--yes"]);
  assert.deepEqual(hook.args, ["hook", "pre-tool-use"]);
  for (const result of [mcp, rustAlias, hook]) {
    assert.equal(result.managed, "1");
    assert.equal(result.selfUpdateDisabled, "1");
    assert.equal(result.packageVersion, PACKAGE_VERSION);
  }
});

test("concurrent launchers populate one atomic artifact", integrationOptions, async (t) => {
  const cache = await temporaryDirectory(t, "contextstream-concurrent-cache");
  const release = await releaseServer(executableFixture("concurrent"), {
    delayArtifactMs: 150,
  });
  t.after(release.close);
  const env = launcherEnvironment(cache, release.prefix);

  const [first, second] = await Promise.all([
    launch(bins.mcp, ["first"], env),
    launch(bins.mcp, ["second"], env),
  ]);
  assert.deepEqual(first.args, ["first"]);
  assert.deepEqual(second.args, ["second"]);
  assert.equal(release.requests.artifact, 1);

  const selected = artifactFor();
  const files = await readdir(
    path.join(cache, `v${PACKAGE_VERSION}`, selected.key),
  );
  assert.deepEqual(files.sort(), ["contextstream-mcp", "metadata.json"]);
});

test("verified cache works offline and ignores a PATH-shadowing binary", integrationOptions, async (t) => {
  const cache = await temporaryDirectory(t, "contextstream-offline-cache");
  const shadow = await temporaryDirectory(t, "contextstream-shadow-path");
  const marker = path.join(shadow, "shadow-ran");
  const shadowBinary = path.join(shadow, "contextstream-mcp");
  await writeFile(shadowBinary, `#!/bin/sh\ntouch ${JSON.stringify(marker)}\nexit 99\n`);
  await chmod(shadowBinary, 0o700);

  const release = await releaseServer(executableFixture("offline"));
  const env = launcherEnvironment(cache, release.prefix, {
    PATH: `${shadow}${path.delimiter}${process.env.PATH}`,
  });
  const online = await launch(bins.mcp, ["online"], env);
  await release.close();
  const offline = await launch(bins.mcp, ["offline"], {
    ...env,
    CONTEXTSTREAM_MCP_OFFLINE: "1",
  });

  assert.equal(online.label, "offline");
  assert.deepEqual(offline.args, ["offline"]);
  await assert.rejects(() => readFile(marker), /ENOENT/u);
});

test("checksum mismatch fails closed without executing the artifact", integrationOptions, async (t) => {
  const cache = await temporaryDirectory(t, "contextstream-tamper-cache");
  const release = await releaseServer(executableFixture("must-not-run"), {
    badChecksum: true,
  });
  t.after(release.close);
  const env = launcherEnvironment(cache, release.prefix);

  await assert.rejects(
    () => execFileAsync(process.execPath, [bins.mcp, "doctor"], { env }),
    (error) => {
      assert.match(error.stderr, /failed SHA-256 verification/u);
      assert.doesNotMatch(error.stdout, /must-not-run/u);
      return true;
    },
  );
});
