import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, it, expect, vi } from "vitest";
import type { SchedulingTolerationPreset } from "@/api/control-layer/types";
import { SchedulingTolerations } from "./SchedulingTolerations";

const PRESETS: SchedulingTolerationPreset[] = [
  {
    id: "no-overflow",
    label: "No overflow capacity",
    description:
      "Never placed on capacity the backend marks with any taint, such as overflow capacity.",
  },
  {
    id: "gpu",
    label: "GPU only",
    description: "Pin to GPU capacity only.",
  },
];

function renderControls(
  preset: string | null | undefined,
  canEdit = true,
  presets: SchedulingTolerationPreset[] | undefined = PRESETS,
) {
  const onChange = vi.fn();
  render(
    <SchedulingTolerations
      preset={preset}
      presets={presets}
      onChange={onChange}
      canEdit={canEdit}
    />,
  );
  return { onChange };
}

describe("SchedulingTolerations", () => {
  it("shows Default when the account has no preset", () => {
    renderControls(null);
    expect(
      screen.getByRole("combobox", { name: /scheduling tolerations preset/i }),
    ).toHaveTextContent("Default (no tolerations pinned)");
  });

  it("shows the configured preset's label when pinned", () => {
    renderControls("gpu");
    expect(
      screen.getByRole("combobox", { name: /scheduling tolerations preset/i }),
    ).toHaveTextContent("GPU only");
    // The description is surfaced as help text.
    expect(screen.getByText("Pin to GPU capacity only.")).toBeInTheDocument();
  });

  it("selects a preset by id", async () => {
    const user = userEvent.setup();
    const { onChange } = renderControls(null);
    await user.click(
      screen.getByRole("combobox", { name: /scheduling tolerations preset/i }),
    );
    await user.click(screen.getByRole("option", { name: "GPU only" }));
    expect(onChange).toHaveBeenCalledWith("gpu");
  });

  it("clears the pin by sending null when Default is chosen", async () => {
    const user = userEvent.setup();
    const { onChange } = renderControls("gpu");
    await user.click(
      screen.getByRole("combobox", { name: /scheduling tolerations preset/i }),
    );
    await user.click(
      screen.getByRole("option", { name: /default \(no tolerations/i }),
    );
    expect(onChange).toHaveBeenCalledWith(null);
  });

  it("disables the select for a non-platform-manager", () => {
    renderControls(null, false);
    expect(
      screen.getByRole("combobox", { name: /scheduling tolerations preset/i }),
    ).toBeDisabled();
  });

  it("surfaces a stored preset that config no longer defines", () => {
    renderControls("retired");
    const alert = screen.getByRole("alert");
    expect(within(alert).getByText(/no longer a configured preset/i)).toBeInTheDocument();
  });

  it("cautions that pinning can make scheduling harder whenever presets exist", () => {
    renderControls(null);
    const caution = screen.getByTestId("toleration-pinning-caution");
    // The qualification matters: tolerations do not force placement, and the
    // effect falls on accounts sharing capacity, so an overbroad "strictly
    // harder for every account" claim must fail this test.
    expect(caution).toHaveTextContent(/can make scheduling harder/i);
    expect(caution).toHaveTextContent(/accounts sharing the same capacity/i);
    expect(caution).toHaveTextContent(/pin as few accounts as possible/i);
    expect(caution).not.toHaveTextContent(/strictly harder for every account/i);
  });

  it("omits the caution when no presets are configured", () => {
    renderControls(null, true, []);
    expect(
      screen.queryByTestId("toleration-pinning-caution"),
    ).not.toBeInTheDocument();
  });
});
