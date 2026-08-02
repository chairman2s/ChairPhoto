import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";
import "@fontsource/inter/800.css";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import LoupeWindow from "./LoupeWindow";

// The same bundle serves both windows. The pop-out loupe window is opened with
// the URL hash "#loupe" (see modules/loupe.ts) and renders only the loupe.
const isLoupe = window.location.hash === "#loupe";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isLoupe ? <LoupeWindow /> : <App />}</React.StrictMode>,
);
