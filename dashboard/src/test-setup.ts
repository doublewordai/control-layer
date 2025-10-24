import "@testing-library/jest-dom";

// Polyfill ResizeObserver for Radix UI and other libs in Vitest/JSDOM
if (typeof window !== "undefined" && !window.ResizeObserver) {
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}


// Mock environment variables
Object.defineProperty(import.meta, "env", {
  value: {
    VITE_API_BASE_URL: undefined,
  },
  writable: true,
});
