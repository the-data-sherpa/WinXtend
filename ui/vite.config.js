import { defineConfig } from "vite";

// Vanilla JS and no framework plugin, so the only thing Vite does here is bundle
// modules and inline the stylesheet. The build has to work offline and produce
// files a strict CSP will load, which rules out anything that injects a remote
// script or an inline one.
export default defineConfig({
  // Tauri prints its own progress over the top of Vite's, and a cleared screen
  // takes the cargo error you were reading with it.
  clearScreen: false,
  server: {
    // Fixed port, and a failure rather than a silent increment: the port is written
    // into tauri.conf.json's devUrl, and a webview pointed at the wrong port shows
    // an empty window with no explanation.
    port: 1420,
    strictPort: true,
    watch: {
      // Rust sources are rebuilt by cargo, not by Vite; watching them makes every
      // `cargo check` reload the page mid-edit.
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    // WebView2 on supported Windows versions and the WebKit builds on macOS and
    // Linux all handle this; going lower only costs bundle size and legibility.
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
  },
});
