import "@testing-library/jest-dom";

// Polyfill ResizeObserver for Radix UI and other libs in Vitest/JSDOM
if (typeof window !== "undefined" && !window.ResizeObserver) {
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}

// Polyfill PointerEvent and hasPointerCapture for Radix UI Select
if (typeof window !== "undefined") {
  // @ts-expect-error - Polyfill for testing
  if (!window.PointerEvent) {
    window.PointerEvent = class PointerEvent extends MouseEvent {} as any;
  }

  // @ts-expect-error - Polyfill for testing
  if (!Element.prototype.hasPointerCapture) {
    Element.prototype.hasPointerCapture = function() {
      return false;
    };
  }

  // @ts-expect-error - Polyfill for testing
  if (!Element.prototype.setPointerCapture) {
    Element.prototype.setPointerCapture = function() {};
  }

  // @ts-expect-error - Polyfill for testing
  if (!Element.prototype.releasePointerCapture) {
    Element.prototype.releasePointerCapture = function() {};
  }

  // @ts-expect-error - Polyfill for testing
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = function() {};
  }
}

// Mock environment variables
Object.defineProperty(import.meta, "env", {
  value: {
    VITE_API_BASE_URL: undefined,
  },
  writable: true,
});
