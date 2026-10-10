import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi } from "vitest";
import { SchedulingRestrictions } from "./SchedulingRestrictions";

function renderControls(
  pinned: Parameters<typeof SchedulingRestrictions>[0]["pinned"],
  canEdit = true,
) {
  const onChange = vi.fn();
  render(
    <SchedulingRestrictions pinned={pinned} onChange={onChange} canEdit={canEdit} />,
  );
  return { onChange };
}

describe("SchedulingRestrictions", () => {
  it("shows the toggle on for the empty-list pin (dedicated capacity)", () => {
    renderControls([]);
    expect(
      screen.getByRole("switch", {
        name: /keep this account's requests on dedicated capacity/i,
      }),
    ).toBeChecked();
  });

  it("shows the toggle off when the account is not pinned", () => {
    renderControls(null);
    expect(
      screen.getByRole("switch", {
        name: /keep this account's requests on dedicated capacity/i,
      }),
    ).not.toBeChecked();
  });

  it("turns the toggle on by sending the empty list", async () => {
    const user = userEvent.setup();
    const { onChange } = renderControls(null);
    await user.click(
      screen.getByRole("switch", {
        name: /keep this account's requests on dedicated capacity/i,
      }),
    );
    expect(onChange).toHaveBeenCalledWith([]);
  });

  it("turns the toggle off by clearing the pin", async () => {
    const user = userEvent.setup();
    const { onChange } = renderControls([]);
    await user.click(
      screen.getByRole("switch", {
        name: /keep this account's requests on dedicated capacity/i,
      }),
    );
    expect(onChange).toHaveBeenCalledWith(null);
  });

  it("hides the controls from a non-platform-manager", () => {
    renderControls(null, false);
    expect(
      screen.queryByRole("button", { name: /advanced/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("switch", {
        name: /keep this account's requests on dedicated capacity/i,
      }),
    ).toBeDisabled();
  });

  it("applies a valid custom list from the advanced editor", async () => {
    const user = userEvent.setup();
    const { onChange } = renderControls(null);
    await user.click(screen.getByRole("button", { name: /advanced/i }));
    const editor = screen.getByLabelText("Scheduling tolerations JSON");
    fireEvent.change(editor, {
      target: { value: '[{"key":"dedicated","value":"only","effect":"NoSchedule"}]' },
    });
    expect(onChange).toHaveBeenLastCalledWith([
      { key: "dedicated", value: "only", effect: "NoSchedule" },
    ]);
  });

  it("surfaces an inline error for an Equal toleration without a value", async () => {
    const user = userEvent.setup();
    const { onChange } = renderControls(null);
    await user.click(screen.getByRole("button", { name: /advanced/i }));
    onChange.mockClear();
    const editor = screen.getByLabelText("Scheduling tolerations JSON");
    fireEvent.change(editor, { target: { value: '[{"key":"dedicated"}]' } });
    const alert = screen.getByRole("alert");
    expect(within(alert).getByText(/Equal needs a value/i)).toBeInTheDocument();
    // The malformed value is never propagated.
    expect(onChange).not.toHaveBeenCalled();
  });

  it("seeds the advanced editor with the current custom list", async () => {
    const user = userEvent.setup();
    renderControls([{ key: "pool", value: "gpu" }]);
    // A custom pin opens the editor already expanded.
    const editor = screen.getByLabelText("Scheduling tolerations JSON");
    expect(editor).toHaveValue(JSON.stringify([{ key: "pool", value: "gpu" }], null, 2));
    await user.click(screen.getByRole("button", { name: /advanced/i }));
  });
});
