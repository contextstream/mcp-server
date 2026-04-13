import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import { loadIgnorePatterns, loadIgnorePatternsSync, getSampleIgnoreContent } from "./ignore.js";

describe("ignore patterns", () => {
  let tempDir: string;

  beforeEach(() => {
    // Create a temporary directory for tests
    tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "cs-ignore-test-"));
  });

  afterEach(() => {
    // Clean up temp directory
    fs.rmSync(tempDir, { recursive: true, force: true });
  });

  describe("loadIgnorePatterns", () => {
    it("should load default patterns when no .contextstream/ignore exists", async () => {
      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.hasUserPatterns).toBe(false);
      // Check that default patterns are loaded
      expect(ig.ignores("node_modules/foo.js")).toBe(true);
      expect(ig.ignores(".git/config")).toBe(true);
      expect(ig.ignores("package-lock.json")).toBe(true);
    });

    it("should load user patterns from .contextstream/ignore", async () => {
      // Create .contextstream/ignore file
      const csDir = path.join(tempDir, ".contextstream");
      fs.mkdirSync(csDir);
      fs.writeFileSync(
        path.join(csDir, "ignore"),
        `# Custom ignore rules
customer-data/
*.secret
src/legacy/
`
      );

      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.hasUserPatterns).toBe(true);
      // Check custom patterns
      expect(ig.ignores("customer-data/file.txt")).toBe(true);
      expect(ig.ignores("config.secret")).toBe(true);
      expect(ig.ignores("src/legacy/old.ts")).toBe(true);
      // Default patterns should still work
      expect(ig.ignores("node_modules/foo.js")).toBe(true);
      // Non-matching paths should not be ignored
      expect(ig.ignores("src/main.ts")).toBe(false);
    });

    it("should handle empty .contextstream/ignore file", async () => {
      const csDir = path.join(tempDir, ".contextstream");
      fs.mkdirSync(csDir);
      fs.writeFileSync(path.join(csDir, "ignore"), "");

      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.hasUserPatterns).toBe(false);
      // Default patterns should still work
      expect(ig.ignores("node_modules/foo.js")).toBe(true);
    });

    it("should ignore comments in .contextstream/ignore", async () => {
      const csDir = path.join(tempDir, ".contextstream");
      fs.mkdirSync(csDir);
      fs.writeFileSync(
        path.join(csDir, "ignore"),
        `# This is a comment
# Another comment
secret.txt
# More comments
`
      );

      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.ignores("secret.txt")).toBe(true);
      expect(ig.ignores("# This is a comment")).toBe(false);
    });
  });

  describe("loadIgnorePatternsSync", () => {
    it("should work synchronously", () => {
      const ig = loadIgnorePatternsSync(tempDir);

      expect(ig.hasUserPatterns).toBe(false);
      expect(ig.ignores("node_modules/foo.js")).toBe(true);
    });
  });

  describe("default ignore patterns", () => {
    it("should ignore version control directories", async () => {
      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.ignores(".git/config")).toBe(true);
      expect(ig.ignores(".svn/entries")).toBe(true);
      expect(ig.ignores(".hg/store")).toBe(true);
    });

    it("should ignore dependency directories", async () => {
      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.ignores("node_modules/lodash/index.js")).toBe(true);
      expect(ig.ignores("vendor/autoload.php")).toBe(true);
    });

    it("should ignore build output directories", async () => {
      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.ignores("dist/bundle.js")).toBe(true);
      expect(ig.ignores("build/main.js")).toBe(true);
      expect(ig.ignores("target/release/app")).toBe(true);
      expect(ig.ignores(".next/cache")).toBe(true);
      expect(ig.ignores(".turbo/state.json")).toBe(true);
      expect(ig.ignores(".parcel-cache/chunk")).toBe(true);
    });

    it("should ignore lock files", async () => {
      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.ignores("package-lock.json")).toBe(true);
      expect(ig.ignores("yarn.lock")).toBe(true);
      expect(ig.ignores("Cargo.lock")).toBe(true);
    });

    it("should not ignore regular source files", async () => {
      const ig = await loadIgnorePatterns(tempDir);

      expect(ig.ignores("src/index.ts")).toBe(false);
      expect(ig.ignores("lib/utils.js")).toBe(false);
      expect(ig.ignores("app/main.py")).toBe(false);
    });
  });

  describe("getSampleIgnoreContent", () => {
    it("should return valid sample content", () => {
      const content = getSampleIgnoreContent();

      expect(content).toContain("# .contextstream/ignore");
      expect(content).toContain("customer-data/");
      expect(content).toContain("**/*.pem");
    });
  });
});
