import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { toast } from "sonner";
import { AddFundsModal } from "./AddCreditsModal";
import * as hooks from "../../../api/control-layer/hooks";
import type { DisplayUser } from "../../../types/display";

// All data/mutation hooks are mocked, so no QueryClientProvider is needed.
vi.mock("../../../api/control-layer/hooks", () => ({
  useAddFunds: vi.fn(),
  useUser: vi.fn(),
}));

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn() },
}));

const targetUser = {
  id: "target-user-id",
  email: "target@example.com",
  display_name: "Target User",
} as DisplayUser;

describe("AddFundsModal", () => {
  const mutateAsync = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    mutateAsync.mockImplementation(async (req) => ({
      id: "txn-1",
      user_id: req.user_id,
      transaction_type: req.transaction_type,
      amount: req.amount,
      source_id: req.source_id,
      description: req.description,
      created_at: "2026-01-01T00:00:00Z",
    }));
    vi.mocked(hooks.useAddFunds).mockReturnValue({
      mutateAsync,
      isPending: false,
    } as never);
    vi.mocked(hooks.useUser).mockReturnValue({
      data: { id: "admin-id", display_name: "Admin Person" },
    } as never);
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  const renderModal = (onClose = vi.fn(), onSuccess = vi.fn()) => {
    const { rerender } = render(
      <AddFundsModal
        isOpen
        onClose={onClose}
        targetUser={targetUser}
        onSuccess={onSuccess}
      />,
    );
    // Mirrors the parent, which keeps the modal mounted and toggles `isOpen`.
    const setOpen = (isOpen: boolean) =>
      rerender(
        <AddFundsModal
          isOpen={isOpen}
          onClose={onClose}
          targetUser={targetUser}
          onSuccess={onSuccess}
        />,
      );
    return { onClose, onSuccess, setOpen };
  };

  // The dialog renders through a portal, so queries use `screen`.
  const dialog = () => screen.getByRole("dialog");

  it("defaults to adding funds", () => {
    renderModal();

    expect(
      screen.getByRole("heading", { name: "Add to Credit Balance" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Add funds" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(
      screen.getByRole("button", { name: "Add to Credit Balance" }),
    ).toBeInTheDocument();
  });

  it("submits an admin_grant with a default gift description", async () => {
    const user = userEvent.setup();
    const { onClose, onSuccess } = renderModal();

    const amount = screen.getByLabelText("Amount (USD)");
    await user.clear(amount);
    await user.type(amount, "25.50");
    await user.click(
      screen.getByRole("button", { name: "Add to Credit Balance" }),
    );

    await waitFor(() => expect(mutateAsync).toHaveBeenCalledTimes(1));
    expect(mutateAsync).toHaveBeenCalledWith(
      expect.objectContaining({
        user_id: "target-user-id",
        amount: 25.5,
        transaction_type: "admin_grant",
        description: "Funds gift from Admin Person",
      }),
    );
    expect(mutateAsync.mock.calls[0][0].source_id).toMatch(/^admin-id_/);
    expect(toast.success).toHaveBeenCalledWith(
      "Successfully added $25.50 to Target User",
    );
    expect(onSuccess).toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });

  it("switches the dialog copy when Remove funds is selected", async () => {
    const user = userEvent.setup();
    renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));

    expect(
      screen.getByRole("heading", { name: "Remove from Credit Balance" }),
    ).toBeInTheDocument();
    expect(dialog()).toHaveTextContent("You are about to remove funds from");
    expect(dialog()).toHaveTextContent(
      "This amount will be deducted from the current balance.",
    );
    expect(
      screen.getByRole("button", { name: "Remove from Credit Balance" }),
    ).toBeInTheDocument();
  });

  it("submits an admin_removal with a positive amount", async () => {
    const user = userEvent.setup();
    const { onClose, onSuccess } = renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));
    const amount = screen.getByLabelText("Amount (USD)");
    await user.clear(amount);
    await user.type(amount, "7");
    await user.click(
      screen.getByRole("button", { name: "Remove from Credit Balance" }),
    );

    await waitFor(() => expect(mutateAsync).toHaveBeenCalledTimes(1));
    expect(mutateAsync).toHaveBeenCalledWith(
      expect.objectContaining({
        user_id: "target-user-id",
        amount: 7,
        transaction_type: "admin_removal",
        description: "Funds removed by Admin Person",
      }),
    );
    expect(toast.success).toHaveBeenCalledWith(
      "Successfully removed $7.00 from Target User",
    );
    expect(onSuccess).toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });

  it("uses a custom description when one is provided", async () => {
    const user = userEvent.setup();
    renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));
    await user.type(
      screen.getByLabelText("Description (optional)"),
      "Refund reversal",
    );
    await user.click(
      screen.getByRole("button", { name: "Remove from Credit Balance" }),
    );

    await waitFor(() => expect(mutateAsync).toHaveBeenCalledTimes(1));
    expect(mutateAsync.mock.calls[0][0].description).toBe("Refund reversal");
  });

  it("rejects a non-positive amount without calling the API", async () => {
    const user = userEvent.setup();
    renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));
    const amount = screen.getByLabelText("Amount (USD)");
    await user.clear(amount);
    await user.type(amount, "0");
    await user.click(
      screen.getByRole("button", { name: "Remove from Credit Balance" }),
    );

    expect(dialog()).toHaveTextContent("Please enter a valid amount");
    expect(mutateAsync).not.toHaveBeenCalled();
  });

  it("shows a mode-specific error when the mutation fails", async () => {
    const user = userEvent.setup();
    mutateAsync.mockRejectedValue(new Error("boom"));
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { onClose, onSuccess } = renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));
    await user.click(
      screen.getByRole("button", { name: "Remove from Credit Balance" }),
    );

    await waitFor(() =>
      expect(dialog()).toHaveTextContent(
        "Failed to remove funds. Please try again.",
      ),
    );
    expect(onSuccess).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("resets to Add funds after cancelling in Remove mode", async () => {
    const user = userEvent.setup();
    const { onClose, setOpen } = renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));
    const amount = screen.getByLabelText("Amount (USD)");
    await user.clear(amount);
    await user.type(amount, "99");
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(1);

    setOpen(false);
    setOpen(true);

    expect(screen.getByRole("tab", { name: "Add funds" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(
      screen.getByRole("heading", { name: "Add to Credit Balance" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("Amount (USD)")).toHaveValue(10);
  });

  it("resets to Add funds after dismissing the dialog with Escape", async () => {
    const user = userEvent.setup();
    const { onClose, setOpen } = renderModal();

    await user.click(screen.getByRole("tab", { name: "Remove funds" }));
    await user.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalledTimes(1);

    setOpen(false);
    setOpen(true);

    expect(screen.getByRole("tab", { name: "Add funds" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });
});
