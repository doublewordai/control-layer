import { render, within, waitFor, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { MemberManagement } from "./MemberManagement";
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

describe("MemberManagement", () => {
  it("shows loading spinner initially", () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );
    expect(container.querySelector(".animate-spin")).toBeInTheDocument();
  });

  it("displays active members", async () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
    });
    expect(within(container).getByText("James Wilson")).toBeInTheDocument();
    expect(
      within(container).getByText("sarah.chen@acme.com"),
    ).toBeInTheDocument();
  });

  it("displays pending invites", async () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(
        within(container).getByText("newuser@acme.com"),
      ).toBeInTheDocument();
    });

    expect(within(container).getByText(/Pending Invites/)).toBeInTheDocument();
  });

  it("shows empty state when no members", async () => {
    server.use(
      http.get("/admin/api/v1/organizations/:orgId/members", () => {
        return HttpResponse.json([]);
      }),
    );

    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("No members yet")).toBeInTheDocument();
    });
  });

  it("shows invite form when clicking Invite Member", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
    });

    await user.click(
      within(container).getByRole("button", { name: /invite member/i }),
    );

    expect(
      within(container).getByPlaceholderText("Enter email address..."),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /send invite/i }),
    ).toBeInTheDocument();
  });

  it("hides management controls in readOnly mode", async () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" readOnly />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
    });

    // No invite button
    expect(
      within(container).queryByRole("button", { name: /invite member/i }),
    ).not.toBeInTheDocument();

    // No remove buttons
    expect(
      within(container).queryByTitle("Remove member"),
    ).not.toBeInTheDocument();
  });

  it("opens remove member dialog", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
    });

    const removeButtons = within(container).getAllByTitle("Remove member");
    await user.click(removeButtons[0]);

    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Remove Member")).toBeInTheDocument();
    expect(within(dialog).getByText(/Sarah Chen/)).toBeInTheDocument();
  });

  it("cancels remove member dialog", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
    });

    const removeButtons = within(container).getAllByTitle("Remove member");
    await user.click(removeButtons[0]);

    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /cancel/i }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });
  });

  it("shows cancel invite button for pending invites", async () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(
        within(container).getByText("newuser@acme.com"),
      ).toBeInTheDocument();
    });

    expect(within(container).getByTitle("Cancel invite")).toBeInTheDocument();
  });

  it("hides cancel invite button in readOnly mode", async () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" readOnly />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(
        within(container).getByText("newuser@acme.com"),
      ).toBeInTheDocument();
    });

    expect(
      within(container).queryByTitle("Cancel invite"),
    ).not.toBeInTheDocument();
  });

  it("shows member count in heading", async () => {
    const { container } = render(
      <MemberManagement organizationId="org-550e8400-0001" />,
      { wrapper: createWrapper() },
    );

    await waitFor(() => {
      expect(within(container).getByText("Members (2)")).toBeInTheDocument();
    });
  });
});
