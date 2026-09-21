import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { toast } from "sonner";
import { AddFundsModal } from "./AddCreditsModal";
import * as hooks from "../../../api/control-layer/hooks";
import type { DisplayUser } from "../../../types/display";

// All data/mutation hooks are mocked at the module boundary, so no
// QueryClientProvider is needed — none of the real useMutation/useQuery
// machinery runs in this file.
vi.mock("../../../api/control-layer/hooks", () => ({
  useAddFunds: vi.fn(),
  useUser: vi.fn(),
}));

vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

const CURRENT_USER_ID = "admin-1";

const mockCurrentUser = {
  id: CURRENT_USER_ID,
  display_name: "Admin User",
  username: "admin",
  email: "admin@example.com",
};

// Build a DisplayUser with only the fields the component actually reads
// (id, display_name, email); the rest is cast to satisfy the type.
const mockTargetUser = {
  id: "user-target-1",
  username: "targetuser",
  email: "target@example.com",
  display_name: "Target User",
} as DisplayUser;

const mockAddFundsResponse = {
  id: "txn-1",
  user_id: "user-target-1",
  transaction_type: "admin_grant",
  amount: 12.5,
  source_id: "admin-1_abc",
  created_at: "2026-01-01T00:00:00Z",
};

const mockMutation = (overrides: { mutateAsync?: any; isPending?: boolean } = {}) => {
  const mutateAsync =
    overrides.mutateAsync ?? vi.fn().mockResolvedValue(mockAddFundsResponse);
  vi.mocked(hooks.useAddFunds).mockReturnValue({
    mutateAsync,
    isPending: overrides.isPending ?? false,
    isError: false,
    error: null,
    isSuccess: false,
    data: undefined,
    mutate: vi.fn(),
    reset: vi.fn(),
    status: "idle",
    context: undefined,
    failureCount: 0,
    failureReason: null,
    isIdle: true,
    isPaused: false,
    submittedAt: 0,
    variables: undefined,
  } as any);
  return mutateAsync;
};

interface RenderArgs {
  isOpen?: boolean;
  onClose?: () => void;
  onSuccess?: () => void;
  targetUser?: DisplayUser;
}

const renderModal = (args: RenderArgs = {}) => {
  const onClose = args.onClose ?? vi.fn();
  const onSuccess = args.onSuccess ?? vi.fn();
  const targetUser = args.targetUser ?? mockTargetUser;
  const utils = render(
    <AddFundsModal
      isOpen={args.isOpen ?? true}
      onClose={onClose}
      onSuccess={onSuccess}
      targetUser={targetUser}
    />,
  );
  return { ...utils, onClose, onSuccess, targetUser };
};

describe("AddFundsModal", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(hooks.useUser).mockReturnValue({
      data: mockCurrentUser,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    } as any);
    mockMutation();
  });

  describe("initial render", () => {
    it("renders the dialog header with the target user's name and email", () => {
      renderModal({ isOpen: true });

      expect(
        screen.getByRole("heading", { name: /add to credit balance/i }),
      ).toBeInTheDocument();
      expect(
        screen.getByText(/target user/i, { exact: false }),
      ).toBeInTheDocument();
      expect(
        screen.getByText(/target@example.com/i, { exact: false }),
      ).toBeInTheDocument();
    });

    it("shows no error banner on a fresh open", () => {
      renderModal({ isOpen: true });
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    });

    it("starts with the default amount of 10.00", () => {
      renderModal({ isOpen: true });
      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      // <input type="number"> exposes its value as a number, so 10.00 -> 10.
      expect(amountInput).toHaveValue(10);
    });
  });

  describe("error display", () => {
    it("shows a validation error when the amount is zero", async () => {
      const user = userEvent.setup();
      renderModal({ isOpen: true });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "0");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() => {
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Please enter a valid amount",
        );
      });
    });

    it("shows a mutation error when addFunds rejects", async () => {
      const user = userEvent.setup();
      mockMutation({ mutateAsync: vi.fn().mockRejectedValue(new Error("boom")) });
      renderModal({ isOpen: true });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "12.50");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() => {
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Failed to add funds. Please try again.",
        );
      });
    });

    it("does not call onSuccess/onClose on validation failure", async () => {
      const user = userEvent.setup();
      const mutateAsync = vi.fn().mockResolvedValue(mockAddFundsResponse);
      mockMutation({ mutateAsync });
      const { onClose, onSuccess } = renderModal({ isOpen: true });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "0");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() =>
        expect(screen.getByRole("alert")).toBeInTheDocument(),
      );
      expect(mutateAsync).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
      expect(onSuccess).not.toHaveBeenCalled();
    });
  });

  describe("stale-error regression (close → reopen lifecycle)", () => {
    // Mirrors how CostManagement.tsx keeps <AddFundsModal> mounted and only
    // toggles `isOpen` via state. The component's useState therefore persists
    // across close→reopen; the fix must ensure a stale `error` is not among
    // that persisted state when the modal is (re)opened.

    it("does not re-show the validation error after close and reopen", async () => {
      const user = userEvent.setup();
      const { rerender, onClose, onSuccess, targetUser } = renderModal({
        isOpen: true,
      });

      // 1. Trigger a validation error (this path does NOT close the dialog).
      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "0");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );
      await waitFor(() =>
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Please enter a valid amount",
        ),
      );

      // 2. Close the modal (parent flips isOpen=false).
      rerender(
        <AddFundsModal
          isOpen={false}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );
      // While closed, Radix DialogContent unmounts so no alert is in the DOM.
      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });

      // 3. Reopen the modal for the same target user.
      rerender(
        <AddFundsModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );

      // 4. The stale validation error must NOT reappear on the freshly opened form.
      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });
    });

    it("does not re-show the mutation error after close and reopen", async () => {
      const user = userEvent.setup();
      mockMutation({ mutateAsync: vi.fn().mockRejectedValue(new Error("boom")) });
      const { rerender, onClose, onSuccess, targetUser } = renderModal({
        isOpen: true,
      });

      // 1. Trigger a mutation error.
      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "12.50");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );
      await waitFor(() =>
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Failed to add funds. Please try again.",
        ),
      );

      // 2. Close.
      rerender(
        <AddFundsModal
          isOpen={false}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );
      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });

      // 3. Reopen with a fresh mutation (no longer rejecting), to also confirm
      //    the error does not depend on the mutation's current disposition.
      mockMutation({ mutateAsync: vi.fn().mockResolvedValue(mockAddFundsResponse) });
      rerender(
        <AddFundsModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );

      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });
    });

    it("does not re-show the error when Cancel is used to close after a failure", async () => {
      const user = userEvent.setup();
      const { onClose, onSuccess, targetUser, rerender } = renderModal({
        isOpen: true,
      });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "0");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );
      await waitFor(() =>
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Please enter a valid amount",
        ),
      );

      // Cancel button drives onClose (same as the parent's flip-to-false).
      await user.click(screen.getByRole("button", { name: /cancel/i }));
      expect(onClose).toHaveBeenCalled();

      rerender(
        <AddFundsModal
          isOpen={false}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );
      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });

      rerender(
        <AddFundsModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );

      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });
    });
  });

  describe("successful submit", () => {
    it("calls mutateAsync with the correct payload, fires a success toast, and invokes onSuccess/onClose", async () => {
      const user = userEvent.setup();
      const mutateAsync = vi.fn().mockResolvedValue(mockAddFundsResponse);
      mockMutation({ mutateAsync });
      const { onClose, onSuccess } = renderModal({ isOpen: true });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "12.50");
      const descriptionInput = screen.getByLabelText(/description/i);
      await user.type(descriptionInput, "Birthday gift");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() => expect(mutateAsync).toHaveBeenCalledTimes(1));
      expect(mutateAsync).toHaveBeenCalledWith(
        expect.objectContaining({
          user_id: "user-target-1",
          amount: 12.5,
          description: "Birthday gift",
          source_id: expect.stringMatching(new RegExp(`^${CURRENT_USER_ID}_`)),
        }),
      );

      await waitFor(() => {
        expect(toast.success).toHaveBeenCalledWith(
          expect.stringMatching(/Successfully added \$12\.50 to Target User/),
        );
        expect(onSuccess).toHaveBeenCalledTimes(1);
        expect(onClose).toHaveBeenCalledTimes(1);
      });
    });

    it("clears the prior error after a successful submit following a failure", async () => {
      const user = userEvent.setup();
      // First mutateAsync rejects, then succeeds (simulating the admin fixing
      // the failure and retrying without closing the modal).
      const mutateAsync = vi.fn().mockRejectedValueOnce(new Error("boom"));
      mutateAsync.mockResolvedValue(mockAddFundsResponse);
      mockMutation({ mutateAsync });
      const { rerender, onClose, onSuccess, targetUser } = renderModal({
        isOpen: true,
      });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "10");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() =>
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Failed to add funds. Please try again.",
        ),
      );

      // Retry without closing — second attempt succeeds.
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() => {
        expect(toast.success).toHaveBeenCalled();
        expect(onClose).toHaveBeenCalled();
      });
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();

      // After close, reopening must still show no stale error banner.
      rerender(
        <AddFundsModal
          isOpen={false}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );
      rerender(
        <AddFundsModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );
      await waitFor(() => {
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      });
    });

    it("resets the amount and description back to defaults after a successful submit", async () => {
      const user = userEvent.setup();
      mockMutation();
      const { rerender, onClose, onSuccess, targetUser } = renderModal({
        isOpen: true,
      });

      const amountInput = screen.getByLabelText(/amount \(usd\)/i);
      await user.clear(amountInput);
      await user.type(amountInput, "42.00");
      const descriptionInput = screen.getByLabelText(/description/i);
      await user.type(descriptionInput, "One-time credit");
      await user.click(
        screen.getByRole("button", { name: /add to credit balance/i }),
      );

      await waitFor(() => expect(onClose).toHaveBeenCalled());

      // Parent reopens the modal again — the form should be reset.
      rerender(
        <AddFundsModal
          isOpen={false}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );
      rerender(
        <AddFundsModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          targetUser={targetUser}
        />,
      );

      await waitFor(() => {
        expect(screen.getByLabelText(/amount \(usd\)/i)).toHaveValue(10);
      });
      expect(screen.getByLabelText(/description/i)).toHaveValue("");
    });
  });

  describe("pending state", () => {
    it("disables the submit and cancel buttons while the mutation is pending", () => {
      mockMutation({ isPending: true });
      renderModal({ isOpen: true });

      const cancelButton = screen.getByRole("button", { name: /cancel/i });
      const submitButton = screen.getByRole("button", { name: /adding\.\.\./i });
      expect(cancelButton).toBeDisabled();
      expect(submitButton).toBeDisabled();
    });

    it("relabels the submit button to 'Adding...' while pending", () => {
      mockMutation({ isPending: true });
      renderModal({ isOpen: true });
      expect(
        screen.getByRole("button", { name: /adding\.\.\./i }),
      ).toBeInTheDocument();
      expect(
        screen.queryByRole("button", { name: /^add to credit balance$/i }),
      ).not.toBeInTheDocument();
    });
  });
});
