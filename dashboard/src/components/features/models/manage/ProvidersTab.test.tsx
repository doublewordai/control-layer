import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";

import type { Model, ModelComponent } from "../../../../api/control-layer";
import { ProvidersTab } from "./ProvidersTab";

const compositeModel: Model = {
  id: "pool",
  alias: "pool",
  model_name: "pool",
  is_composite: true,
  lb_strategy: "weighted_random",
};

const existingComponent: ModelComponent = {
  weight: 50,
  enabled: true,
  sort_order: 7,
  created_at: "2026-08-07T00:00:00Z",
  model: {
    id: "existing-provider",
    alias: "existing-provider",
    model_name: "existing-provider",
  },
};

const candidateModel: Model = {
  id: "candidate-provider",
  alias: "candidate-provider",
  model_name: "candidate-provider",
  hosted_on: "endpoint",
  is_composite: false,
};

let submittedBody: Record<string, unknown> | undefined;

const server = setupServer(
  http.get("/admin/api/v1/models/pool/components", () =>
    HttpResponse.json([existingComponent]),
  ),
  http.get("/admin/api/v1/models", () =>
    HttpResponse.json({
      data: [candidateModel],
      total_count: 1,
      skip: 0,
      limit: 50,
    }),
  ),
  http.get("/admin/api/v1/config", () =>
    HttpResponse.json({ onwards: { strict_mode: false } }),
  ),
  http.post(
    "/admin/api/v1/models/pool/components/candidate-provider",
    async ({ request }) => {
      submittedBody = (await request.json()) as Record<string, unknown>;
      return HttpResponse.json({
        ...existingComponent,
        sort_order: 8,
        model: {
          id: candidateModel.id,
          alias: candidateModel.alias,
          model_name: candidateModel.model_name,
        },
      });
    },
  ),
);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  submittedBody = undefined;
  server.resetHandlers();
});
afterAll(() => server.close());

describe("ProvidersTab", () => {
  it("lets the server assign sort order when adding to weighted routing", async () => {
    const user = userEvent.setup();
    const queryClient = new QueryClient({
      defaultOptions: {
        queries: { retry: false },
        mutations: { retry: false },
      },
    });

    const { container } = render(
      <QueryClientProvider client={queryClient}>
        <ProvidersTab model={compositeModel} canManage={true} />
      </QueryClientProvider>,
    );

    await user.click(
      await within(container).findByRole("button", {
        name: "Add Hosted Model",
      }),
    );

    const dialog = screen.getByRole("dialog");
    await user.click(
      within(dialog).getByRole("combobox", { name: "Select model" }),
    );
    await user.click(await screen.findByText("candidate-provider"));
    await user.click(
      within(dialog).getByRole("button", { name: "Add Hosted Model" }),
    );

    await waitFor(() => expect(submittedBody).toBeDefined());
    expect(submittedBody).toEqual({ weight: 50 });
  });
});
