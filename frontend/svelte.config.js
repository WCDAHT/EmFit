import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
  // Lets <script lang="ts"> and <style lang="..."> work, and runs TS through
  // the same toolchain as the rest of the build.
  preprocess: vitePreprocess(),
};
