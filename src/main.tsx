import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { QuickEntry } from "./QuickEntry";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "./index.css";

async function boot() {
  // In a plain browser during development, stand in for the Rust backend.
  if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) {
    (await import("./dev/fakeBackend")).install();
  }
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      {new URLSearchParams(location.search).has("quick") ? <QuickEntry /> : <App />}
    </React.StrictMode>,
  );
}

boot();
