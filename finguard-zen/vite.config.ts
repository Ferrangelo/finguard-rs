import { defineConfig } from "@lovable.dev/vite-tanstack-config";

export default defineConfig({
  tanstackStart: {
    server: { entry: "server" },
  },
  vite: {
    server: {
      port: 5173,
      proxy: {
        "/api": {
          target: "http://127.0.0.1:3111",
          changeOrigin: true,
        },
      },
    },
  },
});
