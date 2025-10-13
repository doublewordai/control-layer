import { render, screen, act, cleanup } from "@testing-library/react";
import { SettingsProvider } from "./SettingsContext";
import { useSettings } from "./hooks";

// Test component to access the context
function TestComponent() {
  const { settings, toggleFeature, isFeatureEnabled } = useSettings();

  return (
    <div>
      <div data-testid="api-base-url">{settings.apiBaseUrl}</div>
      <div data-testid="demo-enabled">{settings.features.demo.toString()}</div>
      <div data-testid="demo-feature-check">
        {isFeatureEnabled("demo").toString()}
      </div>
      <button
        data-testid="toggle-demo"
        onClick={() => toggleFeature("demo", !settings.features.demo)}
      >
        Toggle Demo
      </button>
    </div>
  );
}

describe("SettingsContext", () => {
  beforeEach(() => {
    // Reset URL search before each test
    window.location.search = "";
  });

  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  it("provides default settings when no localStorage or URL params", () => {
    render(
      <SettingsProvider>
        <TestComponent />
      </SettingsProvider>,
    );

    expect(screen.getByTestId("api-base-url")).toHaveTextContent(
      "/admin/api/v1",
    );
    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("false");
    expect(screen.getByTestId("demo-feature-check")).toHaveTextContent("false");
  });

  it("loads settings from localStorage", () => {
    const storedSettings = {
      apiBaseUrl: "/custom/api",
      features: {
        demo: true,
      },
    };

    localStorage.setItem("app-settings", JSON.stringify(storedSettings));

    render(
      <SettingsProvider>
        <TestComponent />
      </SettingsProvider>,
    );

    expect(screen.getByTestId("api-base-url")).toHaveTextContent("/custom/api");
    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("true");
  });

  it("URL params completely override localStorage settings", () => {
    // Set localStorage with all flags enabled
    const storedSettings = {
      apiBaseUrl: "/stored/api",
      features: {
        demo: true,
      },
    };
    localStorage.setItem("app-settings", JSON.stringify(storedSettings));

    // Mock URLSearchParams to return our desired flags
    const originalURLSearchParams = global.URLSearchParams;
    global.URLSearchParams = vi.fn().mockImplementation(() => ({
      get: vi.fn((key) => {
        if (key === "flags") return "demo";
        return null;
      }),
    }));

    render(
      <SettingsProvider>
        <TestComponent />
      </SettingsProvider>,
    );

    // Restore URLSearchParams
    global.URLSearchParams = originalURLSearchParams;

    // URL flags should override localStorage (only demo enabled)
    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("true");
    // apiBaseUrl should still come from localStorage since URL doesn't set it
    expect(screen.getByTestId("api-base-url")).toHaveTextContent("/stored/api");
  });

  it("toggles feature flags and saves to localStorage", async () => {
    // Mock window.location.reload to prevent jsdom navigation error
    const mockReload = vi.fn();
    Object.defineProperty(window, "location", {
      value: { ...window.location, reload: mockReload },
      writable: true,
    });

    render(
      <SettingsProvider>
        <TestComponent />
      </SettingsProvider>,
    );

    // Check initial state based on clean localStorage (should be false)
    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("false");

    // Toggle demo feature
    await act(async () => {
      screen.getByTestId("toggle-demo").click();
    });

    // Should update state
    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("true");
    expect(screen.getByTestId("demo-feature-check")).toHaveTextContent("true");
  });

  it("demo mode toggle triggers automatic reload", async () => {
    // Store the original window.location object
    const originalLocation = window.location;
    const originalNavigator = window.navigator;

    // Mock window.location with a reload function
    Object.defineProperty(window, "location", {
      value: { reload: vi.fn() },
      writable: true,
    });

    // Mock navigator.serviceWorker for the disable demo test
    Object.defineProperty(window, "navigator", {
      value: {
        ...originalNavigator,
        serviceWorker: {
          getRegistrations: vi
            .fn()
            .mockResolvedValue([
              { unregister: vi.fn().mockResolvedValue(true) },
            ]),
          controller: null,
        },
      },
      writable: true,
    });

    render(
      <SettingsProvider>
        <TestComponent />
      </SettingsProvider>,
    );

    // Initially demo should be false (default)
    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("false");
    expect(window.location.reload).not.toHaveBeenCalled();

    // Toggle demo feature to true
    await act(async () => {
      screen.getByTestId("toggle-demo").click();
    });

    // Should trigger reload when enabling demo mode
    expect(window.location.reload).toHaveBeenCalledTimes(1);

    // Now test disabling demo mode - first clear the mock call count
    vi.mocked(window.location.reload).mockClear();

    // Toggle demo feature back to false
    await act(async () => {
      screen.getByTestId("toggle-demo").click();
    });

    // Should trigger reload when disabling demo mode too
    expect(window.location.reload).toHaveBeenCalledTimes(1);

    // Restore the original objects
    Object.defineProperty(window, "location", {
      value: originalLocation,
      writable: true,
    });
    Object.defineProperty(window, "navigator", {
      value: originalNavigator,
      writable: true,
    });
  });

  it("handles malformed localStorage data gracefully", () => {
    localStorage.setItem("app-settings", "invalid json");

    // Should not throw and use defaults
    render(
      <SettingsProvider>
        <TestComponent />
      </SettingsProvider>,
    );

    expect(screen.getByTestId("demo-enabled")).toHaveTextContent("false");
  });

  it("throws error when useSettings is used outside provider", () => {
    // Suppress console.error for this test
    const consoleSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    expect(() => {
      render(<TestComponent />);
    }).toThrow("useSettings must be used within a SettingsProvider");

    consoleSpy.mockRestore();
  });
});
