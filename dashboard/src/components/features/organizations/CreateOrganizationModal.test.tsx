import { render, within, waitFor, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { ReactNode } from "react";
import { describe, it, expect, vi, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import { CreateOrganizationModal } from "./CreateOrganizationModal";
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

describe("CreateOrganizationModal", () => {
  it("renders form fields when open", () => {
    render(
      <CreateOrganizationModal isOpen={true} onClose={vi.fn()} />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    expect(
      within(dialog).getByText("Create Organization"),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByLabelText("Organization Name"),
    ).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Domain")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Contact Email")).toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", { name: /create/i }),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", { name: /cancel/i }),
    ).toBeInTheDocument();
  });

  it("does not render when closed", () => {
    render(
      <CreateOrganizationModal isOpen={false} onClose={vi.fn()} />,
      { wrapper: createWrapper() },
    );

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("calls onClose when Cancel is clicked", async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(
      <CreateOrganizationModal isOpen={true} onClose={onClose} />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /cancel/i }));

    expect(onClose).toHaveBeenCalled();
  });

  it(
    "creates organization successfully",
    async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      render(
        <CreateOrganizationModal isOpen={true} onClose={onClose} />,
        { wrapper: createWrapper() },
      );

      const dialog = screen.getByRole("dialog");

      // Fill required fields
      await user.type(within(dialog).getByLabelText("Domain"), "new-org");
      await user.type(
        within(dialog).getByLabelText("Contact Email"),
        "admin@new-org.com",
      );
      await user.type(
        within(dialog).getByLabelText("Organization Name"),
        "New Org",
      );

      await user.click(
        within(dialog).getByRole("button", { name: /^create$/i }),
      );

      await waitFor(() => {
        expect(onClose).toHaveBeenCalled();
      });
    },
    10000,
  );

  it("shows owner picker for platform managers", () => {
    render(
      <CreateOrganizationModal
        isOpen={true}
        onClose={vi.fn()}
        isPlatformManager={true}
      />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("Owner")).toBeInTheDocument();
  });

  it("hides owner picker for non-platform-managers", () => {
    render(
      <CreateOrganizationModal isOpen={true} onClose={vi.fn()} />,
      { wrapper: createWrapper() },
    );

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).queryByText("Owner")).not.toBeInTheDocument();
  });
});
