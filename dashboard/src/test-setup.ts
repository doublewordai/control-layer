import "@testing-library/jest-dom";

// Mock environment variables
Object.defineProperty(import.meta, "env", {
  value: {
    VITE_API_BASE_URL: undefined,
  },
  writable: true,
});
