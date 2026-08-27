import "@fontsource/instrument-sans/400.css";
import "@fontsource/instrument-sans/500.css";
import "@fontsource/instrument-sans/600.css";
import "@fontsource/instrument-sans/700.css";
import "@fontsource/instrument-serif/400.css";
import "@fontsource/instrument-serif/400-italic.css";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import LoupeWindow from "./LoupeWindow";
import { applyStandard } from "./theme/apply";

// Stamp ChairPhoto Standard before first paint. App.css's `:root` already carries these same
// values as a no-JS fallback; this re-stamp is what lets Preferences swap the palette at
// runtime later. Both windows (main + the pop-out loupe, see below) go through this file, so
// call it here rather than inside <App> — the loupe window never mounts <App>.
applyStandard();

// The same bundle serves both windows. The pop-out loupe window is opened with
// the URL hash "#loupe" (see modules/loupe.ts) and renders only the loupe.
const isLoupe = window.location.hash === "#loupe";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isLoupe ? <LoupeWindow /> : <App />}</React.StrictMode>,
);
