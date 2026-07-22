import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri reads TAURI_DEV_HOST when developing against a physical mobile device
// or LAN; harmless on desktop.
const host = process.env.TAURI_DEV_HOST;

// The root package.json scripts `cd frontend` before running vite/svelte-check,
// so this dir is the Vite root: svelte.config.js, index.html, and src/ resolve
// relative to it, and the build output lands in frontend/dist (referenced by
// `frontendDist` in src-tauri/tauri.conf.json). See STANDARDS §1.
export default defineConfig({
  plugins: [svelte()],

  // Tauri spawns the Rust process; don't clear its log output from the Vite
  // dev server.
  clearScreen: false,

  server: {
    // Must match `build.devUrl` in src-tauri/tauri.conf.json. strictPort so a
    // port collision fails loudly instead of silently moving the dev server
    // out from under Tauri.
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    fs: {
      // Allow reading files above the frontend root — the About panel imports
      // the repo-root LICENSE.txt via `?raw`. Vite would otherwise block it.
      allow: [".."],
    },
    // The Rust crate has its own rebuild loop; don't let Vite watch it.
    watch: { ignored: ["**/src-tauri/**"] },
  },

  // Produce sourcemaps in debug builds only; keep release bundles lean.
  build: {
    target: "es2021",
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    minify: process.env.TAURI_ENV_DEBUG ? false : "esbuild",
  },
});
