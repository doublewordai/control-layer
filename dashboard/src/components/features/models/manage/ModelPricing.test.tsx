import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import {
  useModelCachePricing,
  useModelOverlays,
  useOrganizationServing,
} from "@/api/control-layer";
import type {
  Model,
  ModelTariff,
  OrganizationServing,
  ServingOverlay,
} from "@/api/control-layer/types";
import { ModelPricing } from "./ModelPricing";
vi.mock("@/api/control-layer", () => ({
  useModelCachePricing: vi.fn(),
  useModelOverlays: vi.fn(),
  useOrganizationServing: vi.fn(),
}));
const tariff = (id: string, extra: Partial<ModelTariff> = {}): ModelTariff => ({
  id,
  name: id,
  deployed_model_id: "model",
  api_key_purpose: "realtime",
  input_price_per_token: "0.000009",
  output_price_per_token: "0.000012",
  valid_from: "2020-01-01T00:00:00Z",
  valid_until: null,
  is_active: true,
  ...extra,
});
const overlay: ServingOverlay = {
  organization_id: "org-a",
  organization_name: "Example Organisation",
  deployed_model_id: "model",
  alias: "example-model",
  default_serving_class: "throughput",
  self_hosted_only: false,
  updated_at: "2020-01-01",
};
const serving: OrganizationServing = {
  organization_id: "org-a",
  granted_serving_classes: ["interactive", "throughput"],
  self_hosted_only: true,
  overlays: [overlay],
  tariffs: [
    tariff("own", {
      organization_id: "org-a",
      input_price_per_token: "0.000002",
    }),
    tariff("interactive", {
      organization_id: "org-a",
      serving_class: "interactive",
      input_price_per_token: "0",
      output_price_per_token: "0",
    }),
  ],
  cache_tariffs: [
    {
      deployed_model_id: "model",
      alias: "example-model",
      serving_class: "interactive",
      read_multiplier: "0.25",
      write_multiplier_5m: "1",
      write_multiplier_1h: "2",
      write_multiplier_24h: "3",
      valid_from: "2020-01-01",
    },
  ],
};
const model: Model = {
  id: "model",
  alias: "example-model",
  model_name: "example-model",
  is_composite: true,
  tariffs: [
    tariff("general"),
    tariff("batch", {
      api_key_purpose: "batch",
      completion_window: "24h",
      input_price_per_token: "0.000001",
    }),
  ],
  serving_classes: {
    interactive: { ttft_ms: 1000, itl_ms: 15, priority: 1 },
    throughput: { ttft_ms: 5000, itl_ms: 60, priority: 0 },
  },
  allowed_batch_completion_windows: ["1h", "24h"],
};
function mount(manager = true, initialOrganization?: string) {
  return render(
    <MemoryRouter>
      <ModelPricing
        model={model}
        manager={manager}
        initialOrganization={initialOrganization}
        onEditPrices={vi.fn()}
        onEditCache={vi.fn()}
      />
    </MemoryRouter>,
  );
}
beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(useModelOverlays).mockReturnValue({
    data: [overlay],
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  } as unknown as ReturnType<typeof useModelOverlays>);
  vi.mocked(useModelCachePricing).mockReturnValue({
    data: {
      enabled: true,
      read_multiplier: "0.1",
      write_multiplier_5m: "1",
      write_multiplier_1h: "2",
      write_multiplier_24h: "3",
      min_prefix_tokens: 1024,
    },
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  } as unknown as ReturnType<typeof useModelCachePricing>);
  vi.mocked(useOrganizationServing).mockReturnValue({
    data: serving,
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  } as unknown as ReturnType<typeof useOrganizationServing>);
});
describe("model pricing selector", () => {
  it("shows effective playground prices when only realtime is configured", () => {
    const { container } = mount(true, "org-a");
    const playground = within(within(container).getByRole("region", { name: "Playground prices" }));
    expect(playground.getByText("Bespoke · interactive · realtime fallback")).toBeInTheDocument();
    expect(playground.getAllByText("$0")).toHaveLength(2);
    expect(playground.getAllByText("$2.00")).toHaveLength(2);
  });

  it("starts general then shows effective class prices and inherited batch/cache values", async () => {
    const user = userEvent.setup();
    const { container } = mount();
    const page = within(container);
    expect(
      page.getByRole("combobox", { name: "Pricing for" }),
    ).toHaveTextContent("General pricing");
    expect(
      within(page.getByRole("region", { name: "Realtime prices" })).getByText(
        "$9.00",
      ),
    ).toBeInTheDocument();
    await user.click(page.getByRole("combobox", { name: "Pricing for" }));
    await user.click(
      within(document.body).getByRole("option", {
        name: "Example Organisation",
      }),
    );
    const rt = within(page.getByRole("region", { name: "Realtime prices" }));
    expect(rt.getByText("Bespoke · interactive")).toBeInTheDocument();
    expect(rt.getAllByText("$0")).toHaveLength(2);
    expect(rt.getAllByText("$2.00")).toHaveLength(2);
    const batch = within(
      page.getByRole("region", { name: "Batch and flex prices" }),
    );
    expect(batch.getByText("Inherited · general model")).toBeInTheDocument();
    expect(batch.getByText("Bespoke · all classes · realtime fallback")).toBeInTheDocument();
    expect(
      page.queryByRole("button", { name: "Manage Tariffs" }),
    ).not.toBeInTheDocument();
    expect(
      page.queryByRole("button", { name: "Edit" }),
    ).not.toBeInTheDocument();
    const cache = within(page.getByRole("region", { name: "Cache pricing" }));
    expect(cache.getByText("0.25×")).toBeInTheDocument();
    expect(cache.getAllByText("Inherited · general model")).toHaveLength(2);
    const sections = [...container.querySelectorAll("section[aria-label]")].map(
      (x) => x.getAttribute("aria-label"),
    );
    expect(sections.indexOf("Cache pricing")).toBeLessThan(
      sections.indexOf("Serving classes"),
    );
    await user.click(page.getByRole("combobox", { name: "Pricing for" }));
    await user.click(
      within(document.body).getByRole("option", { name: "General pricing" }),
    );
    expect(
      within(page.getByRole("region", { name: "Realtime prices" })).getByText(
        "$9.00",
      ),
    ).toBeInTheDocument();
    expect(cache.queryByText("0.25×")).not.toBeInTheDocument();
    expect(cache.getByText("0.1×")).toBeInTheDocument();
  });
  it("shows custom target pricing even without a class-specific tariff", () => {
    vi.mocked(useOrganizationServing).mockReturnValue({
      data: {
        ...serving,
        overlays: [
          {
            ...overlay,
            default_serving_class: null,
            targets: { ttft_ms: 1000, itl_ms: 20, priority: 1 },
          },
        ],
      },
      isLoading: false,
      isError: false,
    } as unknown as ReturnType<typeof useOrganizationServing>);
    const { container } = mount(true, "org-a");
    expect(
      within(
        within(container).getByRole("region", { name: "Realtime prices" }),
      ).getByText("custom"),
    ).toBeInTheDocument();
  });
  it.each(["loading", "error"])(
    "never disguises %s organisation data as general prices",
    (state) => {
      vi.mocked(useOrganizationServing).mockReturnValue({
        data: undefined,
        isLoading: state === "loading",
        isError: state === "error",
        refetch: vi.fn(),
      } as unknown as ReturnType<typeof useOrganizationServing>);
      const { container } = mount(true, "org-a");
      const pricing = within(
        within(container).getByRole("region", { name: "Model pricing" }),
      );
      expect(pricing.queryByText("$9.00")).not.toBeInTheDocument();
      expect(
        pricing.getByRole(state === "loading" ? "status" : "alert"),
      ).toBeInTheDocument();
    },
  );
  it("groups serving and price details in expandable organisation overlays", async () => {
    const user = userEvent.setup();
    const { container } = mount();
    const page = within(container);
    await user.click(
      page.getByText("Example Organisation", { selector: "summary" }),
    );
    expect(await page.findByText("Token price overrides")).toBeInTheDocument();
    expect(page.getByText("No · model override")).toBeInTheDocument();
    expect(page.getByText("Cache multiplier overrides")).toBeInTheDocument();
  });
  it("keeps the selector, class breakdown and privileged reads off customer views", () => {
    const { container } = mount(false, "org-a");
    const page = within(container);
    expect(page.queryByRole("combobox")).not.toBeInTheDocument();
    expect(page.queryByText("Serving classes")).not.toBeInTheDocument();
    expect(useModelOverlays).toHaveBeenCalledWith("model", { enabled: false });
    expect(useOrganizationServing).toHaveBeenCalledWith("", { enabled: false });
  });
});
