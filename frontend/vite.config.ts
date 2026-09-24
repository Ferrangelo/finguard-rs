import { fileURLToPath } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import tsConfigPaths from "vite-tsconfig-paths";

// FINGUARD_SPA switches the build to a static single page app instead of the
// default server-rendered one. It exists for the Tauri Android app, which
// serves static files and has no Node runtime for SSR. The desktop build still
// uses the default SSR build, so this only takes effect when the env var is set
// (see the "build:spa" script in package.json).
const isSpaBuild = process.env.FINGUARD_SPA === "1";

// Keep the dev server on loopback by default because its /api proxy has no
// authentication. FINGUARD_DEV_HOST is an explicit opt-in for another address.
// Vite already rejects non-localhost hostnames by default and admits every
// IP-addressed request no matter what allowedHosts lists, so the bind address is the real control.
const devHost = process.env.FINGUARD_DEV_HOST || "127.0.0.1";

export default defineConfig({
  // Vite uses PostCSS in dev and Lightning CSS only at build, so without this
  // the dev preview can render CSS that the built app renders differently.
  css: { transformer: "lightningcss" },
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
    dedupe: ["react", "react-dom", "react/jsx-runtime", "react/jsx-dev-runtime"],
  },
  // Pre-bundle React up front so a later dependency re-optimization does not
  // invalidate modules that an open tab already loaded.
  optimizeDeps: {
    include: [
      "react",
      "react-dom",
      "react-dom/client",
      "react/jsx-runtime",
      "react/jsx-dev-runtime",
    ],
    ignoreOutdatedRequests: true,
  },
  plugins: [
    tailwindcss(),
    tsConfigPaths({ projects: ["./tsconfig.json"] }),
    tanstackStart({
      // Fail the build when client code imports a server-only module.
      importProtection: {
        behavior: "error",
        client: { files: ["**/server/**"], specifiers: ["server-only"] },
      },
      ...(isSpaBuild
        ? { spa: { enabled: true, prerender: { outputPath: "/index" } } }
        : { server: { entry: "server" } }),
    }),
    viteReact(),
  ],
  server: {
    port: 5173,
    host: devHost,
    // Reload only after a file has stopped changing for a second, so a
    // half-written save does not trigger a failed reload.
    watch: { awaitWriteFinish: { stabilityThreshold: 1000, pollInterval: 100 } },
    proxy: {
      "/api": {
        target: process.env.VITE_API_URL || "http://127.0.0.1:3111",
        changeOrigin: true,
      },
    },
  },
});
