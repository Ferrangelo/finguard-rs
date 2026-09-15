import { defineConfig } from "@lovable.dev/vite-tanstack-config";

// FINGUARD_SPA switches the build to a static single page app instead of the
// default server-rendered one. It exists for the future Tauri Android app,
// which serves static files and has no Node runtime for SSR. Lovable and the
// desktop build still use the default SSR build, so this only takes effect
// when the env var is set (see the "build:spa" script in package.json).
const isSpaBuild = process.env.FINGUARD_SPA === "1";

export default defineConfig({
  tanstackStart: isSpaBuild
    ? { spa: { enabled: true, prerender: { outputPath: "/index" } } }
    : { server: { entry: "server" } },
  vite: {
    server: {
      port: 5173,
      proxy: {
        "/api": {
          // target: "http://127.0.0.1:3111",
          target: process.env.VITE_API_URL || "http://127.0.0.1:3111",
          changeOrigin: true,
        },
      },
    },
  },
});
