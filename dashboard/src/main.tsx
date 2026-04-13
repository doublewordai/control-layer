import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource/space-grotesk/300.css";
import "@fontsource/space-grotesk/400.css";
import "@fontsource/space-grotesk/500.css";
import "@fontsource/space-grotesk/600.css";
import "@fontsource/space-grotesk/700.css";
import "./index.css";
import App from "./App.tsx";
import { captureException, setTelemetryContext } from "./lib/telemetry";

// Emit the build identity as a context event. Telemetry subscribers should
// cache this and merge it into subsequent reports for deploy correlation.
setTelemetryContext({
  build_sha: import.meta.env.VITE_BUILD_SHA ?? "unknown",
  environment: import.meta.env.MODE,
});

// Catch uncaught runtime errors (event handlers, async callbacks, ...).
window.addEventListener("error", (event) => {
  const error =
    event.error ??
    new Error(event.message || "Unknown window error");
  captureException(error, {
    source: "window-error",
    context: {
      filename: event.filename,
      line: event.lineno,
      column: event.colno,
    },
  });
});

// Catch unhandled promise rejections.
window.addEventListener("unhandledrejection", (event) => {
  captureException(event.reason, { source: "unhandled-rejection" });
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
