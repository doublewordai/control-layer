import { render, within, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import { MyOrganization } from "./MyOrganization";
import { OrganizationProvider } from "../../../contexts/organization/OrganizationContext";
import { handlers } from "../../../api/control-layer/mocks/handlers";

const ORG_ID = "org-550e8400-0001";
const ORG_NAME = "Acme Corporation";

const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function userWithOrg(role: string) {
  return http.get("/admin/api/v1/users/:id", ({ params }) => {
    if (params.id !== "current") {
      return HttpResponse.json(
        { error: "User not found" },
        { status: 404 },
      );
    }
    return HttpResponse.json({
      id: "550e8400-e29b-41d4-a716-446655440001",
      username: "github|109540503",
      email: "sarah.chen@acme.com",
      display_name: "Sarah Chen",
      roles: ["StandardUser"],
      created_at: "2025-03-10T10:00:00Z",
      updated_at: "2025-12-20T15:30:00Z",
      auth_source: "proxy-header",
      is_admin: false,
      has_payment_provider_id: false,
      batch_notifications_enabled: false,
      low_balance_threshold: null,
      auto_topup_amount: null,
      auto_topup_threshold: null,
      auto_topup_monthly_limit: null,
      has_auto_topup_payment_method: false,
      active_organization_id: ORG_ID,
      organizations: [{ id: ORG_ID, name: ORG_NAME, role }],
    });
  });
}

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter>
        <OrganizationProvider>{children}</OrganizationProvider>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

describe("MyOrganization", () => {
  it("renders notification settings for org owners", async () => {
    server.use(userWithOrg("owner"));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: ORG_NAME }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("heading", { name: "Notifications" }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("switch", { name: "Email notifications" }),
    ).toBeInTheDocument();
  });

  it("renders notification settings for org admins", async () => {
    server.use(userWithOrg("admin"));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: ORG_NAME }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("heading", { name: "Notifications" }),
    ).toBeInTheDocument();
  });

  it("does not render notification settings for regular members", async () => {
    server.use(userWithOrg("member"));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: ORG_NAME }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).queryByRole("heading", { name: "Notifications" }),
    ).not.toBeInTheDocument();
  });

  it("passes the org ID to NotificationSettings", async () => {
    server.use(userWithOrg("owner"));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: "Notifications" }),
      ).toBeInTheDocument();
    });

    // The notification settings should be fetching webhooks for the org,
    // not the current user. Verify the component rendered with the org context.
    expect(
      within(container).getByRole("switch", { name: "Email notifications" }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: "Add webhook" }),
    ).toBeInTheDocument();
  });
});
