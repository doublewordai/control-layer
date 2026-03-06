import { render, within, waitFor, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { ReactNode } from "react";
import { describe, it, expect, vi, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { EditOrganizationModal } from "./EditOrganizationModal";
import { handlers } from "../../../api/control-layer/mocks/handlers";
import type { Organization } from "../../../api/control-layer/types";

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

const mockOrg: Organization = {
  id: "org-550e8400-0001",
  username: "acme-corp",
  external_user_id: "org|acme-corp",
  email: "admin@acme.com",
  display_name: "Acme Corporation",
  roles: ["StandardUser"],
  created_at: "2025-01-15T10:00:00Z",
  updated_at: "2025-06-01T12:00:00Z",
  auth_source: "proxy-header",
  has_payment_provider_id: false,
  batch_notifications_enabled: false,
  low_balance_threshold: null,
  member_count: 5,
};

describe("EditOrganizationModal", () => {
  it("renders pre-filled form when open", async () => {
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("Edit Organization")).toBeInTheDocument();

    await waitFor(() => {
      expect(within(dialog).getByLabelText("Email")).toHaveValue(
        "admin@acme.com",
      );
    });
    expect(within(dialog).getByLabelText("Display Name")).toHaveValue(
      "Acme Corporation",
    );
  });

  it("does not render when closed", () => {
    render(
      <EditOrganizationModal
        isOpen={false}
        onClose={vi.fn()}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("calls onClose when Cancel is clicked", async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={onClose}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /cancel/i }));

    expect(onClose).toHaveBeenCalled();
  });

  it("updates organization on submit", async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={onClose}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");

    const displayNameInput = within(dialog).getByLabelText("Display Name");
    await user.clear(displayNameInput);
    await user.type(displayNameInput, "Acme Corp Updated");

    await user.click(within(dialog).getByRole("button", { name: /save/i }));

    await waitFor(() => {
      expect(onClose).toHaveBeenCalled();
    });
  });

  it("shows organization username in description", () => {
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText(/acme-corp/)).toBeInTheDocument();
  });
});
