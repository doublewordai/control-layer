import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CachePricingModal } from "./CachePricingModal";
import * as hooks from "../../../api/control-layer/hooks";
import type { CachePricing } from "../../../api/control-layer/types";

// All data/mutation hooks are mocked, so no QueryClientProvider is needed.
vi.mock("../../../api/control-layer/hooks", () => ({
  useModelCachePricing: vi.fn(),
  useUpdateModelCachePricing: vi.fn(),
  useDeleteModelCachePricing: vi.fn(),
}));

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn() },
}));

const enabledPricing: CachePricing = {
  enabled: true,
  write_multiplier_5m: "1.25",
  write_multiplier_1h: "2",
  write_multiplier_24h: "2.5",
  read_multiplier: "0.1",
  min_prefix_tokens: 1024,
  valid_from: "2024-01-01T00:00:00Z",
  valid_until: null,
};

describe("CachePricingModal", () => {
  const updateMock = vi.fn();
  const deleteMock = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    updateMock.mockResolvedValue(undefined);
    deleteMock.mockResolvedValue(undefined);
    vi.mocked(hooks.useUpdateModelCachePricing).mockReturnValue({
      mutateAsync: updateMock,
      isPending: false,
    } as never);
    vi.mocked(hooks.useDeleteModelCachePricing).mockReturnValue({
      mutateAsync: deleteMock,
      isPending: false,
    } as never);
  });

  const setCurrent = (data: CachePricing | undefined) =>
    vi.mocked(hooks.useModelCachePricing).mockReturnValue({
      data,
      isLoading: false,
    } as never);

  const renderModal = (onClose = vi.fn()) => {
    render(
      <CachePricingModal
        isOpen
        modelId="model-1"
        modelName="gpt-4"
        onClose={onClose}
      />,
    );
    return onClose;
  };

  it("disables Save when the form matches saved pricing (no redundant ledger writes)", async () => {
    setCurrent(enabledPricing);
    renderModal();

    // Form initialises from `current`; with no edits there is nothing to save.
    const save = await screen.findByRole("button", { name: /save changes/i });
    expect(save).toBeDisabled();
  });

  it("enables Save and PUTs only the edited multipliers", async () => {
    const user = userEvent.setup();
    setCurrent(enabledPricing);
    renderModal();

    const field = await screen.findByPlaceholderText("e.g. 1.25");
    await user.clear(field);
    await user.type(field, "1.5");

    const save = screen.getByRole("button", { name: /save changes/i });
    expect(save).toBeEnabled();
    await user.click(save);

    await waitFor(() => expect(updateMock).toHaveBeenCalledTimes(1));
    expect(updateMock).toHaveBeenCalledWith(
      expect.objectContaining({
        modelId: "model-1",
        data: expect.objectContaining({ write_multiplier_5m: "1.5" }),
      }),
    );
    expect(deleteMock).not.toHaveBeenCalled();
  });

  it("disables cache pricing (DELETE) when toggled off", async () => {
    const user = userEvent.setup();
    setCurrent(enabledPricing);
    renderModal();

    const toggle = await screen.findByRole("switch");
    await user.click(toggle); // turn cache pricing off

    const save = screen.getByRole("button", { name: /save changes/i });
    expect(save).toBeEnabled();
    await user.click(save);

    await waitFor(() => expect(deleteMock).toHaveBeenCalledWith("model-1"));
    expect(updateMock).not.toHaveBeenCalled();
  });
});
