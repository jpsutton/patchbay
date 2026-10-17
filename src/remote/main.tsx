import React from "react";
import ReactDOM from "react-dom/client";
import { RemoteApp } from "./RemoteApp";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "../index.css";
import "./remote.css";

async function boot() {
  // In a plain browser during development, stand in for the phone's backend.
  if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) {
    (await import("../dev/fakeBackend")).install();
  }
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <RemoteApp />
    </React.StrictMode>,
  );
}

boot();
