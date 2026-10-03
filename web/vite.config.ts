import { defineConfig } from "vitest/config";
import preact from "@preact/preset-vite";

// Served from https://octos-org.github.io/octos-crucible/
export default defineConfig({
  base: "/octos-crucible/",
  plugins: [preact()],
  // config/keys.json lives one level up and is bundled into the page.
  server: { fs: { allow: [".."] } },
  build: { target: "es2022" },
  test: {
    environment: "node",
    include: ["test/**/*.test.ts"],
    testTimeout: 30000,
  },
});
