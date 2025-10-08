/// <reference types="vitest" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test-setup.ts"],
    exclude: [
      "**/node_modules/**",
      "**/dist/**",
      "**/e2e/**",
      "**/.{git,cache,output,temp}/**",
      "**/{karma,rollup,webpack,vite,vitest,jest,ava,babel,nyc,cypress,tsup,build,eslint,prettier}.config.*",
    ],
    coverage: {
      provider: "v8",
      reporter: ["text", "lcov", "html", "cobertura"],
      exclude: [
        "node_modules/",
        "src/test-setup.ts",
        "**/*.config.{js,ts}",
        "**/*.d.ts",
        "**/*.test.{js,ts,tsx}",
        "**/*.spec.{js,ts,tsx}",
        "**/mocks/**",
        "dist/",
        "e2e/",
        "public/",
        "src/components/ui/**", // Exclude shadcn/ui components
      ],
      thresholds: {
        lines: 15,
        functions: 40,
        branches: 60,
        statements: 15,
      },
    },
  },
});
