import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import App from "./App.tsx";
import { PostHogProvider } from "posthog-js/react";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    {import.meta.env.MODE === "development" ? (
      <App />
    ) : (
      <PostHogProvider
        apiKey="phc_Gsc7iKs7KCkxySkNRu8pmJb3dYr4RcrOgYPvnBmvviH"
        options={{
          api_host: "https://eu.i.posthog.com",
          defaults: "2025-05-24",
          capture_exceptions: true, // This enables capturing exceptions using Error Tracking
          debug: false,
        }}
      >
        <App />
      </PostHogProvider>
    )}
  </StrictMode>,
);
