import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { QuickEntry } from "./QuickEntry";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "./index.css";

async function boot() {
  // Outside Tauri: a fake backend during development, else patchbayd serving this page.
  if (!("__TAURI_INTERNALS__" in window)) {
    if (import.meta.env.DEV) (await import("./dev/fakeBackend")).install();
    else await (await import("./lib/webBackend")).install();
  }
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      {new URLSearchParams(location.search).has("quick") ? <QuickEntry /> : <App />}
    </React.StrictMode>,
  );
}

boot();
