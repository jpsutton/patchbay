// The phone app's frontend (remote.html), built into dist-remote/ for remote/src-tauri.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  build: {
    outDir: "dist-remote",
    rollupOptions: { input: "remote.html" },
  },
  server: {
    port: 1424,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1425 } : undefined,
    // `remote/**` is the phone's Rust crate. `**/remote/**` also matches src/remote, so the phone UI never reloaded.
    watch: { ignored: ["**/src-tauri/**", "**/vendor/**", "**/target/**", "remote/**"] },
  },
});
