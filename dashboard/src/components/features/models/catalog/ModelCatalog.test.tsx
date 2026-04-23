import { render, waitFor, within, fireEvent } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import {
  describe,
  it,
  expect,
  beforeAll,
  afterEach,
  afterAll,
  vi,
} from "vitest";
import { ModelCatalog } from "./ModelCatalog";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import { SettingsProvider } from "../../../../contexts/settings/SettingsContext";

vi.mock("../../../../utils/authorization", () => ({
  useAuthorization: vi.fn(() => ({
    userRoles: ["StandardUser"],
    hasPermission: vi.fn(() => false),
    canAccessRoute: vi.fn(() => true),
  })),
}));

const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  sessionStorage.clear();
  localStorage.clear();
});
afterAll(() => server.close());

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return ({ children }: { children: ReactNode }) => (
    <SettingsProvider>
      <QueryClientProvider client={queryClient}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    </SettingsProvider>
  );
}

describe("ModelCatalog", () => {
  it("renders the heading and featured releases", async () => {
    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: /^models$/i }),
      ).toBeInTheDocument();
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: /featured releases/i }),
      ).toBeInTheDocument();
    });
  });

  it("collapses variants behind a family row by default", async () => {
    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    const familyRow = await waitFor(() =>
      within(container).getByRole("button", {
        name: /expand qwen 3\.5 variants/i,
      }),
    );

    expect(familyRow).toHaveAttribute("aria-expanded", "false");
  });

  it("expands a family when the chevron is clicked", async () => {
    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    const expandButton = await waitFor(() =>
      within(container).getByRole("button", {
        name: /expand qwen 3\.5 variants/i,
      }),
    );

    fireEvent.click(expandButton);

    await waitFor(() => {
      expect(
        within(container).getByRole("button", {
          name: /collapse qwen 3\.5 variants/i,
        }),
      ).toBeInTheDocument();
    });
  });

  it("exposes the async / batch pricing toggle", async () => {
    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: /^models$/i }),
      ).toBeInTheDocument();
    });

    const asyncTab = within(container).getByRole("tab", {
      name: /async pricing/i,
    });
    const batchTab = within(container).getByRole("tab", {
      name: /batch pricing/i,
    });

    expect(asyncTab).toHaveAttribute("aria-selected", "true");
    expect(batchTab).toHaveAttribute("aria-selected", "false");

    fireEvent.click(batchTab);

    expect(asyncTab).toHaveAttribute("aria-selected", "false");
    expect(batchTab).toHaveAttribute("aria-selected", "true");
  });

  it("shows provider and capability filter dropdowns", async () => {
    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByLabelText(/filter by provider/i),
      ).toBeInTheDocument();
    });
    expect(
      within(container).getByLabelText(/filter by capability/i),
    ).toBeInTheDocument();
  });

  it("renders the empty state message when no models are returned", async () => {
    server.use(
      http.get("/admin/api/v1/models", () => {
        return HttpResponse.json({ data: [], total: 0 });
      }),
    );

    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByText(/no models available/i),
      ).toBeInTheDocument();
    });
  });

  it("supports fuzzy search via the search input", async () => {
    const { container } = render(<ModelCatalog />, {
      wrapper: createWrapper(),
    });

    const input = await waitFor(() =>
      within(container).getByLabelText(/search models/i),
    );

    fireEvent.change(input, { target: { value: "qwen" } });
    expect(input).toHaveValue("qwen");
  });
});
