import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { ModelTariffTable } from "./ModelTariffTable";

describe("ModelTariffTable", () => {
  it("offers an explicit background tariff independently of foreground SLAs", async () => {
    const user = userEvent.setup();
    render(
      <ModelTariffTable
        tariffs={[]}
        onChange={vi.fn()}
        availableSLAs={["1h", "24h"]}
      />,
    );

    await user.click(screen.getByRole("button", { name: /add tariff/i }));
    await user.click(screen.getAllByRole("combobox")[0]);
    await user.click(await screen.findByRole("option", { name: "Batch" }));
    await user.click(screen.getAllByRole("combobox")[1]);

    const listbox = await screen.findByRole("listbox");
    expect(
      within(listbox).getByRole("option", { name: "Background" }),
    ).toBeInTheDocument();
  });
});
