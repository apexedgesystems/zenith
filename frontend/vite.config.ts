/// <reference types="vitest" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      "/api": {
        target: "http://localhost:8080",
        changeOrigin: true,
        ws: true,
      },
    },
  },
  build: {
    outDir: "dist",
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
    css: false,
    // Measured only when asked (make coverage-frontend runs vitest with
    // --coverage). Every source file is listed, so a file no test loads
    // reports 0% instead of dropping out of the report.
    coverage: {
      provider: "v8",
      include: ["src/**/*.{ts,tsx}"],
      exclude: ["src/**/*.test.{ts,tsx}", "src/test/**", "src/**/*.d.ts"],
      reportsDirectory: "../coverage/frontend",
      reporter: [
        "text-summary",
        ["text", { file: "summary.txt", maxCols: 120 }],
        ["html", { subdir: "html" }],
        "lcovonly",
      ],
    },
  },
});
