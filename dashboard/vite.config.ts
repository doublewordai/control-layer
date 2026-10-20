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
        "@": path.resolve(import.meta.dirname, "./src"),
      },
    },
    server: {
      allowedHosts: ["clay-frontend", "localhost", "host.docker.internal"],
      hmr: {
        protocol: "ws",
        port: 5173,
        host: "localhost",
      },
      proxy: {
        "/admin": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
          configure: (proxy) => {
            proxy.on("proxyReq", (proxyReq) => {
              if (process.env.AUTHORIZATION) {
                proxyReq.setHeader(
                  "authorization",
                  `Bearer ${process.env.AUTHORIZATION}`,
                );
              }
            });
          },
        },
        "/authentication": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
          configure: (proxy) => {
            proxy.on("proxyReq", (proxyReq) => {
              if (process.env.AUTHORIZATION) {
                proxyReq.setHeader(
                  "authorization",
                  `Bearer ${process.env.AUTHORIZATION}`,
                );
              }
            });
          },
        },
        "/ai": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
          configure: (proxy) => {
            proxy.on("proxyReq", (proxyReq) => {
              if (process.env.AUTHORIZATION) {
                proxyReq.setHeader(
                  "authorization",
                  `Bearer ${process.env.AUTHORIZATION}`,
                );
              }
            });
          },
        },
        "/openai-openapi.yaml": {
          target: process.env.BACKEND_URL || "http://localhost:3001",
          changeOrigin: true,
          configure: (proxy) => {
            proxy.on("proxyReq", (proxyReq) => {
              if (process.env.AUTHORIZATION) {
                proxyReq.setHeader(
                  "authorization",
                  `Bearer ${process.env.AUTHORIZATION}`,
                );
              }
            });
          },
        },
      },
    },
    build: {
      // Don't emit Vite's module-preload polyfill as an inline <script>.
      // When served behind a strict Content-Security-Policy with
      // `script-src 'self'` (no 'unsafe-inline'), an inline polyfill script is
      // blocked and breaks the app. Evergreen browsers support
      // <link rel="modulepreload"> natively, so the polyfill is unnecessary.
      modulePreload: { polyfill: false },
      rollupOptions: {
        output: {
          manualChunks(id) {
            const moduleId = id.replaceAll("\\", "/");

            if (
              moduleId.includes("/node_modules/react/") ||
              moduleId.includes("/node_modules/react-dom/")
            ) {
              return "react-core";
            }

            if (
              moduleId.includes("/node_modules/react-router-dom/") ||
              moduleId.includes("/node_modules/react-router/")
            ) {
              return "react-router";
            }

            if (
              moduleId.includes("/node_modules/@tanstack/react-query/") ||
              moduleId.includes("/node_modules/@tanstack/query-core/")
            ) {
              return "react-query";
            }

            if (moduleId.includes("/node_modules/@radix-ui/")) {
              return "radix-ui";
            }
          },
        },
      },
    },
  };
});
