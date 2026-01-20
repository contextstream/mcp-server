/** @type {import("vitest/config").UserConfig} */
module.exports = {
  // In some environments node_modules is not writable; avoid Vite writing temp files there.
  cacheDir: ".vite",
  test: {
    globals: true,
    environment: "node",
    include: ["src/**/*.test.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text", "json", "html"],
      exclude: ["node_modules", "dist", "**/*.test.ts"],
    },
  },
};

