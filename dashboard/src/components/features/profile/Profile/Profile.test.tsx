import { render, within, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { Profile } from "./Profile";
import { handlers } from "../../../../api/control-layer/mocks/handlers";

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

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
