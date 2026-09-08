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
import { toast } from "sonner";

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
  Toaster: () => null,
}));

const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  vi.clearAllMocks();
});
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
  auto_topup_amount: null,
  auto_topup_threshold: null,
  has_auto_topup_payment_method: false,
  auto_topup_monthly_limit: null,
  zero_data_retention: false,
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

  it("reports pending verification instead of success when the email is changed", async () => {
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
    const emailInput = within(dialog).getByLabelText("Email");
    await user.clear(emailInput);
    await user.type(emailInput, "new-contact@acme.com");

    await user.click(within(dialog).getByRole("button", { name: /save/i }));

    await waitFor(() => {
      expect(onClose).toHaveBeenCalled();
    });
    expect(toast.info).toHaveBeenCalledWith(
      "Email change pending verification",
      expect.objectContaining({
        description: expect.stringContaining("new-contact@acme.com"),
      }),
    );
    expect(toast.info).toHaveBeenCalledWith(
      expect.any(String),
      expect.objectContaining({
        description: expect.stringContaining("admin@acme.com"),
      }),
    );
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("shows plain success when only non-email fields change", async () => {
    const user = userEvent.setup();
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    const displayNameInput = within(dialog).getByLabelText("Display Name");
    await user.clear(displayNameInput);
    await user.type(displayNameInput, "Renamed");
    await user.click(within(dialog).getByRole("button", { name: /save/i }));

    await waitFor(() => {
      expect(toast.success).toHaveBeenCalled();
    });
    expect(toast.info).not.toHaveBeenCalled();
  });

  it("shows the pending email change notice when one is in flight", () => {
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        organization={{
          ...mockOrg,
          pending_email_change: {
            new_email: "pending@acme.com",
            expires_at: "2025-06-02T12:00:00Z",
          },
        }}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    const notice = within(dialog).getByRole("alert");
    expect(notice).toHaveTextContent(/pending@acme.com/);
    expect(notice).toHaveTextContent(
      /waiting on both admin@acme.com and pending@acme.com/i,
    );
  });

  it("names only the outstanding address once one side has confirmed", () => {
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        organization={{
          ...mockOrg,
          pending_email_change: {
            new_email: "pending@acme.com",
            expires_at: "2025-06-02T12:00:00Z",
            new_email_confirmed_at: "2025-06-01T13:00:00Z",
            old_email_confirmed_at: null,
          },
        }}
      />,
      { wrapper: createWrapper() },
    );

    const notice = within(screen.getByRole("dialog")).getByRole("alert");
    expect(notice).toHaveTextContent(/waiting on admin@acme.com to confirm/i);
    expect(notice).not.toHaveTextContent(/both/i);
  });

  it("does not show a pending notice when nothing is in flight", () => {
    render(
      <EditOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        organization={mockOrg}
      />,
      { wrapper: createWrapper() },
    );

    expect(
      within(screen.getByRole("dialog")).queryByRole("alert"),
    ).not.toBeInTheDocument();
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
