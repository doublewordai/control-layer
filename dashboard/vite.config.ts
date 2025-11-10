import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";
import tailwindcss from "@tailwindcss/vite";
import { visualizer } from "rollup-plugin-visualizer";

// https://vite.dev/config/
export default defineConfig(({ mode }) => {
  loadEnv(mode, process.cwd(), "");

  return {
    plugins: [
      react(),
      tailwindcss(),
      visualizer({
        open: false,
        filename: "dist/stats.html",
        gzipSize: true,
        brotliSize: true,
      }),
    ],
    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
      },
    },
    server: {
      allowedHosts: ["clay-frontend", "localhost"],
      hmr: {
        protocol: "ws",
        port: 5173,
        host: "localhost",
      },
      proxy: {
        "/admin": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
        },
        "/authentication": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
        },
        "/ai": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
        },
        "/openai-openapi.yaml": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
        },
      },
    },
  };
});
