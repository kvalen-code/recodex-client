import react from "@vitejs/plugin-react-swc"; // recodex-overlay:react-swc
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  build: {
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes("node_modules")) return;
          if (id.includes("node_modules/react") || id.includes("node_modules/scheduler")) return "react";
          if (id.includes("node_modules/@tauri-apps")) return "tauri";
          return "vendor";
        },
      },
    },
  }, // recodex-overlay:manual-chunks
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
});
