import { mount } from "svelte";
import "./styles/global.css";
import { installConsoleBridge } from "./lib/log";
import { initTheme } from "./lib/theme";
import App from "./App.svelte";

// Mirror every frontend console.* into the Rust log file before anything else
// runs, so even early startup logs are captured.
installConsoleBridge();

// Apply the persisted/preferred theme before first paint (STANDARDS §2.2).
initTheme();

const app = mount(App, {
  // The non-null assertion is safe: #app is declared in index.html.
  target: document.getElementById("app")!,
});

export default app;
