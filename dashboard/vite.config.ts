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
      rollupOptions: {
        output: {
          manualChunks: {
            // Split React core libraries for better caching
            "react-core": ["react", "react-dom"],
            "react-router": ["react-router-dom"],
            // Split TanStack Query for better caching
            "react-query": ["@tanstack/react-query"],
            // Split Radix UI components
            "radix-ui": [
              "@radix-ui/react-dialog",
              "@radix-ui/react-dropdown-menu",
              "@radix-ui/react-select",
              "@radix-ui/react-tabs",
              "@radix-ui/react-tooltip",
              "@radix-ui/react-popover",
              "@radix-ui/react-avatar",
              "@radix-ui/react-label",
              "@radix-ui/react-checkbox",
              "@radix-ui/react-switch",
            ],
          },
        },
      },
    },
  };
});
