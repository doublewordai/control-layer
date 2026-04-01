import { render, within, waitFor, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { Organizations } from "./Organizations";
import { handlers } from "../../../api/control-layer/mocks/handlers";

const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter>{children}</MemoryRouter>
    </QueryClientProvider>
  );
}

describe("Organizations", () => {
  it("renders heading and create button", async () => {
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: "Organizations" }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("button", { name: /create organization/i }),
    ).toBeInTheDocument();
  });

  it("displays organization data when loaded", async () => {
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    expect(within(container).getByText("Widgets Inc")).toBeInTheDocument();
    expect(within(container).getByText("admin@acme.com")).toBeInTheDocument();
    expect(within(container).getByText("admin@widgets.io")).toBeInTheDocument();
  });

  it("shows empty table when no organizations exist", async () => {
    server.use(
      http.get("/admin/api/v1/organizations", () => {
        return HttpResponse.json({
          data: [],
          total_count: 0,
          skip: 0,
          limit: 10,
        });
      }),
    );

    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: "Organizations" }),
      ).toBeInTheDocument();
    });

    // No org names should appear
    expect(
      within(container).queryByText("Acme Corporation"),
    ).not.toBeInTheDocument();
  });

  it("has a search input", async () => {
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByPlaceholderText("Search organizations..."),
      ).toBeInTheDocument();
    });
  });

  it("opens create organization modal", async () => {
    const user = userEvent.setup();
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: "Organizations" }),
      ).toBeInTheDocument();
    });

    await user.click(
      within(container).getByRole("button", { name: /create organization/i }),
    );

    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByText("Create Organization"),
    ).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Domain")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Contact Email")).toBeInTheDocument();
  });

  it("opens edit organization modal via action button", async () => {
    const user = userEvent.setup();
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    const editButtons = within(container).getAllByTitle("Edit organization");
    await user.click(editButtons[0]);

    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Edit Organization")).toBeInTheDocument();
  });

  it("opens and cancels delete confirmation dialog", async () => {
    const user = userEvent.setup();
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    const deleteButtons = within(container).getAllByTitle(
      "Delete organization",
    );
    await user.click(deleteButtons[0]);

    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Delete Organization")).toBeInTheDocument();
    expect(within(dialog).getByText(/Acme Corporation/)).toBeInTheDocument();

    // Cancel should close dialog
    await user.click(within(dialog).getByRole("button", { name: /cancel/i }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });
  });

  it("deletes an organization", async () => {
    const user = userEvent.setup();
    const { container } = render(<Organizations />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByText("Acme Corporation"),
      ).toBeInTheDocument();
    });

    const deleteButtons = within(container).getAllByTitle(
      "Delete organization",
    );
    await user.click(deleteButtons[0]);

    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /^delete$/i }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });
  });
});
