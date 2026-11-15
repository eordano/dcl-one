import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
// @ts-expect-error -- plain JS helper, shared with the two vite configs so the
import { validateAlias } from "./vite.validate.js";

const ui = (file: string) => fileURLToPath(new URL("../../library/ui/" + file, import.meta.url));

export default defineConfig({
  resolve: {
    alias: [
      ...validateAlias(),
      { find: "@catalyst-api", replacement: fileURLToPath(new URL("../../library/catalyst-api", import.meta.url)) },
      { find: "@shop", replacement: fileURLToPath(new URL("../../library/shop", import.meta.url)) },
      // The Explorer HUD UI (library/ui/apps/hud) and the behaviours /play supplies to it.
      { find: "@hud-host", replacement: fileURLToPath(new URL("./src/play/host.ts", import.meta.url)) },
      { find: "@hud", replacement: fileURLToPath(new URL("../../library/ui/apps/hud", import.meta.url)) },
      { find: "@explorer-core", replacement: fileURLToPath(new URL("../../library/explorer-core", import.meta.url)) },
      { find: /^@dcl\/one-ui$/, replacement: ui("index.ts") },
      { find: /^@dcl\/one-ui\//, replacement: ui("") },
    ],
    // library/ has no node_modules of its own: viem, React and the testing packages the shared HUD's test
    // harness imports resolve to ui3's copies.
    dedupe: ["viem", "react", "react-dom", "@noble/curves", "@noble/hashes", "@testing-library/react", "@testing-library/user-event", "@testing-library/dom", "@testing-library/jest-dom"],
  },
  // The HUD UI lives outside this app; its assets load through the dev server like ui3's own.
  server: { fs: { allow: [".", fileURLToPath(new URL("../../library/ui/apps/hud", import.meta.url)), fileURLToPath(new URL("../../library/explorer-core", import.meta.url))] } },
  plugins: [react()],
  test: {
    environment: "jsdom",
    globals: true,
    // HUD tests render the whole shell with lazy chunks; the default 5 s fails under shared-machine load.
    testTimeout: 15000,
    execArgv: ["--no-experimental-webstorage"],
    projects: [
      {
        extends: true,
        test: {
          name: "ui3",
          include: ["src/**/*.test.{ts,tsx}"],
          exclude: ["src/play/**"],
          setupFiles: ["src/test/setup.ts"],
          css: true,
        },
      },
      {
        // The Shop's portable core (library/shop), which every Shop host shares.
        extends: true,
        test: {
          name: "shop-core",
          dir: "../../library/shop",
          include: ["**/*.test.ts"],
          environment: "node",
        },
      },
      {
        // The Explorer HUD (src/play) under the harness it was written for upstream
        // (react-web/vitest.config.ts): its own setup file, no CSS processing.
        extends: true,
        test: {
          name: "play",
          include: ["src/play/**/*.test.{ts,tsx}"],
          setupFiles: ["src/play/test/setup.ts"],
          css: false,
        },
      },
      {
        // The shared HUD UI's and the Explorer page core's tests against /play's host (src/play/host.ts);
        // react-web runs the same tests against its own.
        extends: true,
        test: {
          name: "play-shared",
          include: ["../../library/ui/apps/hud/**/*.test.{ts,tsx}", "../../library/explorer-core/**/*.test.{ts,tsx}"],
          setupFiles: ["src/play/test/setup.ts"],
          css: false,
          sequence: { hooks: "list" },
        },
      },
    ],
  },
});
