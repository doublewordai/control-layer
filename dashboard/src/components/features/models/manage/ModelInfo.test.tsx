import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { Model } from "../../../../api/control-layer";
import {
  useConfig,
  useDaemons,
  useEndpoint,
  useModel,
  useModelCachePricing,
  useModelComponents,
  useProbes,
  useUpdateModel,
} from "../../../../api/control-layer";
import { useAuthorization } from "../../../../utils";
import ModelInfo from "./ModelInfo";

vi.mock("../../../../api/control-layer", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../../../api/control-layer")>()),
  useConfig: vi.fn(),
  useDaemons: vi.fn(),
  useEndpoint: vi.fn(),
  useModel: vi.fn(),
  useModelCachePricing: vi.fn(),
  useModelComponents: vi.fn(),
  useProbes: vi.fn(),
  useUpdateModel: vi.fn(),
}));

vi.mock("../../../../utils", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../../../utils")>()),
  useAuthorization: vi.fn(),
}));

vi.mock("../../../modals", () => ({
  AccessManagementModal: () => null,
  ApiExamples: () => null,
  CachePricingModal: () => null,
  DeleteVirtualModelModal: () => null,
  UpdateModelPricingModal: () => null,
}));

vi.mock("../../../ui/card", () => ({
  Card: ({ children }: { children: ReactNode }) => <section>{children}</section>,
  CardContent: ({ children }: { children: ReactNode }) => <div>{children}</div>,
  CardDescription: ({ children }: { children: ReactNode }) => (
    <p>{children}</p>
  ),
  CardHeader: ({ children }: { children: ReactNode }) => (
    <header>{children}</header>
  ),
  CardTitle: ({ children }: { children: ReactNode }) => <h2>{children}</h2>,
}));

vi.mock("./ModelProbes", () => ({ default: () => null }));
vi.mock("./ProvidersTab", () => ({ default: () => null }));
vi.mock("./UserUsageTable", () => ({ default: () => null }));

const virtualModel: Model = {
  id: "virtual-model-id",
  alias: "zai-org/GLM-5.2-FP8",
  display_name: "GLM 5.2",
  model_name: "zai-org/GLM-5.2-FP8",
  model_type: "CHAT",
  capabilities: ["reasoning"],
  is_composite: true,
  reasoning_translation_overrides: null,
};

describe("ModelInfo", () => {
  const updateModel = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    updateModel.mockResolvedValue(virtualModel);

    vi.mocked(useAuthorization).mockReturnValue({
      hasPermission: vi.fn(() => true),
    } as unknown as ReturnType<typeof useAuthorization>);
    vi.mocked(useModel).mockReturnValue({
      data: virtualModel,
      isLoading: false,
      error: null,
    } as ReturnType<typeof useModel>);
    vi.mocked(useEndpoint).mockReturnValue({
      data: undefined,
      isLoading: false,
      error: null,
    } as ReturnType<typeof useEndpoint>);
    vi.mocked(useUpdateModel).mockReturnValue({
      mutateAsync: updateModel,
      isPending: false,
    } as unknown as ReturnType<typeof useUpdateModel>);
    vi.mocked(useModelCachePricing).mockReturnValue({
      data: undefined,
      isLoading: false,
    } as ReturnType<typeof useModelCachePricing>);
    vi.mocked(useProbes).mockReturnValue({ data: [] } as unknown as ReturnType<
      typeof useProbes
    >);
    vi.mocked(useModelComponents).mockReturnValue({
      data: [],
    } as unknown as ReturnType<typeof useModelComponents>);
    vi.mocked(useDaemons).mockReturnValue({
      data: { daemons: [] },
    } as unknown as ReturnType<typeof useDaemons>);
    vi.mocked(useConfig).mockReturnValue({
      data: {
        batches: { allowed_completion_windows: ["24h"] },
        onwards: { strict_mode: false },
      },
    } as unknown as ReturnType<typeof useConfig>);
  });

  it("omits standard-only reasoning translation overrides when saving a virtual model", async () => {
    const user = userEvent.setup();
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const { container } = render(
      <QueryClientProvider client={queryClient}>
        <MemoryRouter initialEntries={["/models/manage/virtual-model-id"]}>
          <Routes>
            <Route path="/models/manage/:modelId" element={<ModelInfo />} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    const detailsHeading = within(container).getByRole("heading", {
      name: "Model Details",
    });
    await user.click(within(detailsHeading.parentElement!).getByRole("button"));
    await user.click(
      within(container).getByRole("button", { name: "Save Changes" }),
    );

    await waitFor(() => expect(updateModel).toHaveBeenCalledOnce());
    const request = updateModel.mock.calls[0][0];
    expect(request.data).not.toHaveProperty("reasoning_translation_overrides");
  });
});
