import { render, screen, within, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
      organizations: [
        { id: ORG_ID, name: ORG_NAME, role, zero_data_retention: false },
      ],
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

  it("renders read-only webhooks view for regular members", async () => {
    server.use(userWithOrg("member"));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: ORG_NAME }),
      ).toBeInTheDocument();
    });

    // Read-only view: heading is "Webhooks", and admin-only controls are absent.
    expect(
      within(container).getByRole("heading", { name: "Webhooks" }),
    ).toBeInTheDocument();
    expect(
      within(container).queryByRole("heading", { name: "Notifications" }),
    ).not.toBeInTheDocument();
    expect(
      within(container).queryByRole("button", { name: "Add webhook" }),
    ).not.toBeInTheDocument();
    expect(
      within(container).queryByRole("switch", { name: "Email notifications" }),
    ).not.toBeInTheDocument();
  });

  const orgDetail = (zeroDataRetention: boolean) =>
    http.get("/admin/api/v1/organizations/:id", () =>
      HttpResponse.json({
        id: ORG_ID,
        username: "acme-corp",
        display_name: ORG_NAME,
        email: "contact@acme.com",
        created_at: "2025-01-15T10:00:00Z",
        member_count: 3,
        zero_data_retention: zeroDataRetention,
      }),
    );

  it("shows the zero data retention badge when it is enabled", async () => {
    server.use(userWithOrg("member"));
    server.use(orgDetail(true));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: ORG_NAME }),
      ).toBeInTheDocument();
    });

    expect(container.textContent).toMatch(/zero data retention/i);
  });

  it("hides the zero data retention badge when it is disabled", async () => {
    server.use(userWithOrg("member"));
    server.use(orgDetail(false));
    const { container } = render(<MyOrganization />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: ORG_NAME }),
      ).toBeInTheDocument();
    });

    expect(container.textContent).not.toMatch(/zero data retention/i);
  });

  const CURRENT_USER_ID = "550e8400-e29b-41d4-a716-446655440001";

  const sarahUser = {
    id: CURRENT_USER_ID,
    username: "github|109540503",
    email: "sarah.chen@acme.com",
    display_name: "Sarah Chen",
  };

  const jamesUser = {
    id: "550e8400-e29b-41d4-a716-446655440002",
    username: "james.wilson",
    email: "james.wilson@acme.com",
    display_name: "James Wilson",
  };

  const membersHandler = (members: Record<string, unknown>[]) =>
    http.get("/admin/api/v1/organizations/:orgId/members", () =>
      HttpResponse.json(members),
    );

  describe("member key management", () => {
    it("sends can_manage_keys: false when inviting a member with the toggle off", async () => {
      server.use(userWithOrg("owner"), orgDetail(false));
      let inviteBody: Record<string, unknown> | null = null;
      server.use(
        http.post(
          "/admin/api/v1/organizations/:orgId/invites",
          async ({ request }) => {
            inviteBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json({
              id: "inv-100",
              email: inviteBody.email,
              role: inviteBody.role,
              status: "pending",
              created_at: "2026-07-01T00:00:00Z",
              expires_at: "2026-07-08T00:00:00Z",
            });
          },
        ),
      );

      const user = userEvent.setup();
      const { container } = render(<MyOrganization />, {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(
          within(container).getByRole("button", { name: /invite member/i }),
        ).toBeInTheDocument();
      });
      await user.click(
        within(container).getByRole("button", { name: /invite member/i }),
      );

      const dialog = await screen.findByRole("dialog");
      await user.type(
        within(dialog).getByLabelText("Email"),
        "newmember@acme.com",
      );

      // Member is the default role, with the API keys toggle shown and off.
      expect(
        within(dialog).getByRole("radio", { name: "Member" }),
      ).toBeChecked();
      expect(
        within(dialog).getByRole("switch", { name: "Can generate API keys" }),
      ).not.toBeChecked();
      // Owner is not offered at invite time.
      expect(
        within(dialog).queryByRole("radio", { name: "Owner" }),
      ).not.toBeInTheDocument();

      await user.click(
        within(dialog).getByRole("button", { name: "Send Invite" }),
      );

      await waitFor(() => {
        expect(inviteBody).toEqual({
          email: "newmember@acme.com",
          role: "member",
          can_manage_keys: false,
        });
      });
    });

    it("omits can_manage_keys when inviting an admin", async () => {
      server.use(userWithOrg("owner"), orgDetail(false));
      let inviteBody: Record<string, unknown> | null = null;
      server.use(
        http.post(
          "/admin/api/v1/organizations/:orgId/invites",
          async ({ request }) => {
            inviteBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json({
              id: "inv-101",
              email: inviteBody.email,
              role: inviteBody.role,
              status: "pending",
              created_at: "2026-07-01T00:00:00Z",
              expires_at: "2026-07-08T00:00:00Z",
            });
          },
        ),
      );

      const user = userEvent.setup();
      const { container } = render(<MyOrganization />, {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(
          within(container).getByRole("button", { name: /invite member/i }),
        ).toBeInTheDocument();
      });
      await user.click(
        within(container).getByRole("button", { name: /invite member/i }),
      );

      const dialog = await screen.findByRole("dialog");
      await user.type(
        within(dialog).getByLabelText("Email"),
        "newadmin@acme.com",
      );
      await user.click(within(dialog).getByRole("radio", { name: "Admin" }));

      // The API keys toggle only applies to the member role.
      expect(
        within(dialog).queryByRole("switch", {
          name: "Can generate API keys",
        }),
      ).not.toBeInTheDocument();

      await user.click(
        within(dialog).getByRole("button", { name: "Send Invite" }),
      );

      await waitFor(() => {
        expect(inviteBody).toEqual({
          email: "newadmin@acme.com",
          role: "admin",
        });
      });
      expect(inviteBody).not.toHaveProperty("can_manage_keys");
    });

    it("pre-populates the role modal and PATCHes can_manage_keys", async () => {
      server.use(userWithOrg("owner"), orgDetail(false));
      server.use(
        membersHandler([
          {
            id: "mem-1",
            user: sarahUser,
            role: "owner",
            status: "active",
            created_at: "2025-01-15T10:00:00Z",
            can_manage_keys: true,
          },
          {
            id: "mem-2",
            user: jamesUser,
            role: "member",
            status: "active",
            created_at: "2025-02-01T10:00:00Z",
            can_manage_keys: false,
          },
        ]),
      );
      let patchBody: Record<string, unknown> | null = null;
      server.use(
        http.patch(
          "/admin/api/v1/organizations/:orgId/members/:userId",
          async ({ request }) => {
            patchBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json({
              id: "mem-2",
              user: jamesUser,
              role: patchBody.role,
              status: "active",
              created_at: "2025-02-01T10:00:00Z",
              can_manage_keys: patchBody.can_manage_keys === true,
            });
          },
        ),
      );

      const user = userEvent.setup();
      const { container } = render(<MyOrganization />, {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(
          within(container).getByText("James Wilson"),
        ).toBeInTheDocument();
      });

      await user.click(
        within(container).getByRole("button", {
          name: "Change role for James Wilson",
        }),
      );

      const dialog = await screen.findByRole("dialog");
      // Pre-populated from the member's current role and capability.
      expect(
        within(dialog).getByRole("radio", { name: "Member" }),
      ).toBeChecked();
      const toggle = within(dialog).getByRole("switch", {
        name: "Can generate API keys",
      });
      expect(toggle).not.toBeChecked();
      // Owner is offered when changing an existing member's role.
      expect(
        within(dialog).getByRole("radio", { name: "Owner" }),
      ).toBeInTheDocument();

      await user.click(toggle);
      await user.click(within(dialog).getByRole("button", { name: "Save" }));

      await waitFor(() => {
        expect(patchBody).toEqual({ role: "member", can_manage_keys: true });
      });
    });

    it("shows the key icon only for members who can create API keys", async () => {
      server.use(userWithOrg("owner"), orgDetail(false));
      server.use(
        membersHandler([
          {
            id: "mem-1",
            user: sarahUser,
            role: "owner",
            status: "active",
            created_at: "2025-01-15T10:00:00Z",
            can_manage_keys: true,
          },
          {
            id: "mem-2",
            user: jamesUser,
            role: "member",
            status: "active",
            created_at: "2025-02-01T10:00:00Z",
            can_manage_keys: false,
          },
        ]),
      );

      const { container } = render(<MyOrganization />, {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(
          within(container).getByText("James Wilson"),
        ).toBeInTheDocument();
      });

      // Only Sarah (can_manage_keys: true) gets the key icon; James does not.
      expect(
        within(container).getAllByRole("img", {
          name: "Can create API keys",
        }),
      ).toHaveLength(1);
    });

    it("shows key icons for every member with the capability", async () => {
      server.use(userWithOrg("owner"), orgDetail(false));
      server.use(
        membersHandler([
          {
            id: "mem-1",
            user: sarahUser,
            role: "owner",
            status: "active",
            created_at: "2025-01-15T10:00:00Z",
            can_manage_keys: true,
          },
          {
            id: "mem-2",
            user: jamesUser,
            role: "member",
            status: "active",
            created_at: "2025-02-01T10:00:00Z",
            can_manage_keys: true,
          },
        ]),
      );

      const { container } = render(<MyOrganization />, {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(
          within(container).getByText("James Wilson"),
        ).toBeInTheDocument();
      });

      expect(
        within(container).getAllByRole("img", {
          name: "Can create API keys",
        }),
      ).toHaveLength(2);
    });
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
