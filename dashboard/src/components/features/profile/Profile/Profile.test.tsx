import { render, screen, within, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { Profile } from "./Profile";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import type { Webhook } from "../../../../api/control-layer/types";

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

/** Helper: wait for Profile to finish loading. */
async function renderAndWaitForProfile() {
  const user = userEvent.setup();
  const result = render(<Profile />, { wrapper: createWrapper() });
  await waitFor(() => {
    expect(
      within(result.container).getByRole("heading", {
        name: "Profile Settings",
      }),
    ).toBeInTheDocument();
  });
  return { ...result, user };
}

/** Factory for a fully-formed Webhook object. */
function makeWebhook(overrides: Partial<Webhook> = {}): Webhook {
  return {
    id: "wh-test-1",
    user_id: "550e8400-e29b-41d4-a716-446655440001",
    url: "https://example.com/webhook",
    enabled: true,
    event_types: ["batch.completed", "batch.failed"],
    description: "My test webhook",
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
    disabled_at: null,
    ...overrides,
  };
}

// Test wrapper with QueryClient and Router
function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false, // Disable retries for tests
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter>{children}</MemoryRouter>
    </QueryClientProvider>
  );
}

describe("Profile Component", () => {
  it("renders without crashing", async () => {
    const { container } = render(<Profile />, { wrapper: createWrapper() });

    // Should show loading skeleton initially
    expect(document.querySelector(".animate-pulse")).toBeInTheDocument();

    // Should render the component after loading
    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: "Profile Settings" }),
      ).toBeInTheDocument();
    });
  });

  it("renders profile data when loaded", async () => {
    const { container } = render(<Profile />, { wrapper: createWrapper() });

    await waitFor(() => {
      // Check that the page header is displayed
      expect(
        within(container).getByRole("heading", { name: "Profile Settings" }),
      ).toBeInTheDocument();
      expect(
        within(container).getByText(
          /Manage your account information and preferences/,
        ),
      ).toBeInTheDocument();
    });

    // Check that user data from mock is displayed
    expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
    expect(
      within(container).getAllByText("sarah.chen@doubleword.ai")[0],
    ).toBeInTheDocument();

    // Check form fields are accessible via roles
    expect(
      within(container).getByRole("textbox", { name: "Display Name" }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("textbox", { name: "Avatar URL" }),
    ).toBeInTheDocument();

    // Check buttons are accessible
    expect(
      within(container).getByRole("button", { name: /save changes/i }),
    ).toBeInTheDocument();
  });

  it("allows editing display name and avatar url", async () => {
    const user = userEvent.setup();
    const { container } = render(<Profile />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: "Profile Settings" }),
      ).toBeInTheDocument();
    });

    // Find form fields by their roles and accessible names
    const displayNameInput = within(container).getByRole("textbox", {
      name: "Display Name",
    });
    const avatarUrlInput = within(container).getByRole("textbox", {
      name: "Avatar URL",
    });

    // Verify initial values (from mock data)
    expect(displayNameInput).toHaveValue("Sarah Chen");
    expect(avatarUrlInput).toHaveValue("/avatars/user-1.png");

    // Edit display name
    await user.clear(displayNameInput);
    await user.type(displayNameInput, "Sarah J. Chen");

    // Edit avatar URL
    await user.clear(avatarUrlInput);
    await user.type(avatarUrlInput, "https://example.com/new-avatar.jpg");

    // Verify the inputs have been updated
    expect(displayNameInput).toHaveValue("Sarah J. Chen");
    expect(avatarUrlInput).toHaveValue("https://example.com/new-avatar.jpg");

    // Find and click save button by role, not by text
    const saveButton = within(container).getByRole("button", {
      name: /save changes/i,
    });
    expect(saveButton).not.toBeDisabled();

    await user.click(saveButton);

    // Verify success state appears
    await waitFor(() => {
      expect(
        within(container).getByText(/Profile updated successfully/i),
      ).toBeInTheDocument();
    });
  });
});

describe("Email Notifications", () => {
  it("renders the email notifications toggle", async () => {
    const { container } = await renderAndWaitForProfile();

    expect(within(container).getByText("Notifications")).toBeInTheDocument();
    expect(
      within(container).getByText("Email Notifications"),
    ).toBeInTheDocument();
    expect(
      within(container).getByText(
        /Receive email when a batch completes or fails/,
      ),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("switch", { name: "Email notifications" }),
    ).toBeInTheDocument();
  });

  it("reflects the current user preference", async () => {
    // Override the current user to have email notifications enabled
    server.use(
      http.get("/admin/api/v1/users/:id", () => {
        return HttpResponse.json({
          id: "550e8400-e29b-41d4-a716-446655440001",
          username: "sarah.chen",
          email: "sarah.chen@doubleword.ai",
          display_name: "Sarah Chen",
          avatar_url: "/avatars/user-1.png",
          roles: ["PlatformManager", "RequestViewer"],
          created_at: "2024-01-10T10:00:00Z",
          updated_at: "2024-01-20T15:30:00Z",
          auth_source: "vouch",
          batch_notifications_enabled: true,
        });
      }),
    );

    const { container } = await renderAndWaitForProfile();
    const toggle = within(container).getByRole("switch", {
      name: "Email notifications",
    });
    expect(toggle).toHaveAttribute("data-state", "checked");
  });

  it("calls the API when toggled", async () => {
    let patchedData: Record<string, unknown> | null = null;

    server.use(
      http.patch("/admin/api/v1/users/:id", async ({ request }) => {
        patchedData = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({
          id: "550e8400-e29b-41d4-a716-446655440001",
          username: "sarah.chen",
          email: "sarah.chen@doubleword.ai",
          display_name: "Sarah Chen",
          avatar_url: "/avatars/user-1.png",
          roles: ["PlatformManager", "RequestViewer"],
          created_at: "2024-01-10T10:00:00Z",
          updated_at: "2024-01-20T15:30:00Z",
          auth_source: "vouch",
          batch_notifications_enabled: true,
        });
      }),
    );

    const { container, user } = await renderAndWaitForProfile();
    const toggle = within(container).getByRole("switch", {
      name: "Email notifications",
    });

    await user.click(toggle);

    await waitFor(() => {
      expect(patchedData).toEqual({ batch_notifications_enabled: true });
    });

    // "Saved" confirmation text appears briefly
    await waitFor(() => {
      expect(within(container).getByText("Saved")).toBeInTheDocument();
    });
  });
});

describe("Webhooks", () => {
  it("shows empty state when there are no webhooks", async () => {
    const { container } = await renderAndWaitForProfile();

    expect(within(container).getByText("Webhooks")).toBeInTheDocument();
    expect(
      within(container).getByText(
        /No webhooks configured\. Add one to receive HTTP notifications\./,
      ),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: "Add webhook" }),
    ).toBeInTheDocument();
  });

  it("displays existing webhooks", async () => {
    const webhook = makeWebhook();
    server.use(
      http.get("/admin/api/v1/users/:userId/webhooks", () => {
        return HttpResponse.json([webhook]);
      }),
    );

    const { container } = await renderAndWaitForProfile();

    // URL is shown
    await waitFor(() => {
      expect(
        within(container).getByText("https://example.com/webhook"),
      ).toBeInTheDocument();
    });

    // Description is shown
    expect(
      within(container).getByText("My test webhook"),
    ).toBeInTheDocument();

    // Event type badges
    expect(
      within(container).getByText("batch.completed"),
    ).toBeInTheDocument();
    expect(within(container).getByText("batch.failed")).toBeInTheDocument();

    // Action buttons
    expect(
      within(container).getByRole("button", {
        name: `Edit webhook ${webhook.url}`,
      }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", {
        name: `Delete webhook ${webhook.url}`,
      }),
    ).toBeInTheDocument();
  });

  it("shows a disabled badge for disabled webhooks", async () => {
    server.use(
      http.get("/admin/api/v1/users/:userId/webhooks", () => {
        return HttpResponse.json([makeWebhook({ enabled: false })]);
      }),
    );

    const { container } = await renderAndWaitForProfile();

    await waitFor(() => {
      expect(within(container).getByText("Disabled")).toBeInTheDocument();
    });
  });

  it("shows warning icon for auto-disabled webhooks", async () => {
    server.use(
      http.get("/admin/api/v1/users/:userId/webhooks", () => {
        return HttpResponse.json([
          makeWebhook({ disabled_at: "2025-06-01T00:00:00Z", enabled: false }),
        ]);
      }),
    );

    const { container } = await renderAndWaitForProfile();

    await waitFor(() => {
      expect(
        within(container).getByText(
          "Auto-disabled due to repeated delivery failures.",
        ),
      ).toBeInTheDocument();
    });
  });

  it("opens the create webhook dialog and validates URL", async () => {
    const { container, user } = await renderAndWaitForProfile();

    // Click "Add Webhook" button
    await user.click(
      within(container).getByRole("button", { name: "Add webhook" }),
    );

    // Dialog opens (rendered as a portal)
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Add Webhook")).toBeInTheDocument();
    expect(
      within(dialog).getByText(
        /Configure a URL to receive HTTP POST notifications/,
      ),
    ).toBeInTheDocument();

    // Event types are checked by default
    const checkboxes = within(dialog).getAllByRole("checkbox");
    expect(checkboxes).toHaveLength(2);
    checkboxes.forEach((cb) => {
      expect(cb).toHaveAttribute("data-state", "checked");
    });

    // Try to create with empty URL -> validation error
    await user.click(within(dialog).getByRole("button", { name: "Create Webhook" }));
    await waitFor(() => {
      expect(within(dialog).getByText("URL is required")).toBeInTheDocument();
    });

    // Try with an invalid URL
    const urlInput = within(dialog).getByLabelText("Endpoint URL");
    await user.type(urlInput, "not-a-url");
    await user.click(within(dialog).getByRole("button", { name: "Create Webhook" }));
    await waitFor(() => {
      expect(
        within(dialog).getByText("Please enter a valid URL"),
      ).toBeInTheDocument();
    });

    // Try with an http:// URL
    await user.clear(urlInput);
    await user.type(urlInput, "http://example.com/hook");
    await user.click(within(dialog).getByRole("button", { name: "Create Webhook" }));
    await waitFor(() => {
      expect(
        within(dialog).getByText("URL must use HTTPS"),
      ).toBeInTheDocument();
    });
  });

  it("creates a webhook and shows the secret", async () => {
    const { container, user } = await renderAndWaitForProfile();

    await user.click(
      within(container).getByRole("button", { name: "Add webhook" }),
    );

    const dialog = await screen.findByRole("dialog");

    // Fill in valid URL
    const urlInput = within(dialog).getByLabelText("Endpoint URL");
    await user.type(urlInput, "https://example.com/my-hook");

    // Fill in description
    const descInput = within(dialog).getByLabelText("Description (optional)");
    await user.type(descInput, "Test description");

    // Submit
    await user.click(
      within(dialog).getByRole("button", { name: "Create Webhook" }),
    );

    // After creation, the secret is displayed
    await waitFor(() => {
      expect(
        within(dialog).getByText("Webhook Created"),
      ).toBeInTheDocument();
    });
    expect(
      within(dialog).getByText(/Copy this secret now/),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", { name: "Copy secret" }),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", { name: "Done" }),
    ).toBeInTheDocument();
  });

  it("opens the edit dialog pre-filled with webhook data", async () => {
    const webhook = makeWebhook({
      url: "https://hooks.slack.com/abc",
      description: "Slack notifications",
    });
    server.use(
      http.get("/admin/api/v1/users/:userId/webhooks", () => {
        return HttpResponse.json([webhook]);
      }),
    );

    const { container, user } = await renderAndWaitForProfile();

    // Wait for webhook to render
    await waitFor(() => {
      expect(
        within(container).getByText("https://hooks.slack.com/abc"),
      ).toBeInTheDocument();
    });

    // Click edit
    await user.click(
      within(container).getByRole("button", {
        name: `Edit webhook ${webhook.url}`,
      }),
    );

    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Edit Webhook")).toBeInTheDocument();

    // Fields are pre-filled
    expect(within(dialog).getByLabelText("Endpoint URL")).toHaveValue(
      "https://hooks.slack.com/abc",
    );
    expect(
      within(dialog).getByLabelText("Description (optional)"),
    ).toHaveValue("Slack notifications");

    // Rotate Secret button is available in edit mode
    expect(
      within(dialog).getByRole("button", { name: /Rotate Secret/ }),
    ).toBeInTheDocument();
  });

  it("shows delete confirmation dialog and deletes a webhook", async () => {
    const webhook = makeWebhook();
    server.use(
      http.get("/admin/api/v1/users/:userId/webhooks", () => {
        return HttpResponse.json([webhook]);
      }),
    );

    const { container, user } = await renderAndWaitForProfile();

    await waitFor(() => {
      expect(
        within(container).getByText(webhook.url),
      ).toBeInTheDocument();
    });

    // Click delete
    await user.click(
      within(container).getByRole("button", {
        name: `Delete webhook ${webhook.url}`,
      }),
    );

    // Confirm dialog appears
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Delete Webhook")).toBeInTheDocument();
    expect(
      within(dialog).getByText(
        /Are you sure you want to delete this webhook\?/,
      ),
    ).toBeInTheDocument();

    // Cancel closes dialog
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });

    // Open again and confirm delete
    await user.click(
      within(container).getByRole("button", {
        name: `Delete webhook ${webhook.url}`,
      }),
    );
    const dialog2 = await screen.findByRole("dialog");
    await user.click(
      within(dialog2).getByRole("button", { name: "Delete" }),
    );

    // Dialog closes after deletion
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });
  });

});
