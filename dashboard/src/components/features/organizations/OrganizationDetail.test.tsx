import { render, within, waitFor, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { OrganizationDetail } from "./OrganizationDetail";
import { OrganizationProvider } from "../../../contexts/organization/OrganizationContext";
import { handlers } from "../../../api/control-layer/mocks/handlers";

const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function createWrapper(orgId: string) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[`/organizations/${orgId}`]}>
        <OrganizationProvider>
          <Routes>
            <Route
              path="/organizations/:organizationId"
              element={children}
            />
          </Routes>
        </OrganizationProvider>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

describe("OrganizationDetail", () => {
  it("shows loading spinner initially", () => {
    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-550e8400-0001"),
    });
    expect(container.querySelector(".animate-spin")).toBeInTheDocument();
  });

  it("renders organization details", async () => {
    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-550e8400-0001"),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    expect(within(container).getByText("admin@acme.com")).toBeInTheDocument();
    expect(within(container).getByText(/5 members/)).toBeInTheDocument();
    expect(within(container).getByText("$250.00")).toBeInTheDocument();
  });

  it("shows not found for missing organization", async () => {
    server.use(
      http.get("/admin/api/v1/organizations/:id", () => {
        return HttpResponse.json({ error: "Not found" }, { status: 404 });
      }),
    );

    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-nonexistent"),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Organization not found."),
      ).toBeInTheDocument();
    });
  });

  it("has a Back button", async () => {
    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-550e8400-0001"),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("button", { name: /back/i }),
    ).toBeInTheDocument();
  });

  it("has an Edit button", async () => {
    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-550e8400-0001"),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("button", { name: /edit/i }),
    ).toBeInTheDocument();
  });

  it("opens edit modal when Edit is clicked", async () => {
    const user = userEvent.setup();
    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-550e8400-0001"),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    await user.click(
      within(container).getByRole("button", { name: /edit/i }),
    );

    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Edit Organization")).toBeInTheDocument();
  });

  it("renders member management section", async () => {
    const { container } = render(<OrganizationDetail />, {
      wrapper: createWrapper("org-550e8400-0001"),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    // MemberManagement renders Members heading
    await waitFor(() => {
      expect(within(container).getByText(/^Members/)).toBeInTheDocument();
    });
  });
});
