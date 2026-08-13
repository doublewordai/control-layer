import { afterEach, describe, expect, it, vi } from "vitest";
import { dwctlApi } from "../client";
import { ApiError } from "../errors";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("model component client", () => {
  it("preserves the requested tier when adding a provider", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          weight: 25,
          enabled: true,
          sort_order: 2,
          created_at: "2026-08-07T00:00:00Z",
          model: { id: "provider", alias: "provider", model_name: "provider" },
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    await dwctlApi.models.components.add("pool", {
      deployed_model_id: "provider",
      weight: 25,
      enabled: true,
      sort_order: 2,
    });

    expect(fetchMock).toHaveBeenCalledWith(
      "/admin/api/v1/models/pool/components/provider",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ weight: 25, enabled: true, sort_order: 2 }),
      }),
    );
  });

  it("sends a single exact-set routing update", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response("[]", {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const layout = {
      components: [
        {
          deployed_model_id: "provider",
          weight: 25,
          enabled: true,
          sort_order: 0,
        },
      ],
    };

    await dwctlApi.models.components.updateLayout("pool", layout);

    expect(fetchMock).toHaveBeenCalledWith(
      "/admin/api/v1/models/pool/components/routing",
      expect.objectContaining({ method: "PUT", body: JSON.stringify(layout) }),
    );
  });

  it("preserves conflict status for stale exact-set updates", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response("Component set changed", { status: 409 }),
      ),
    );

    const request = dwctlApi.models.components.updateLayout("pool", {
      components: [],
    });

    await expect(request).rejects.toEqual(
      expect.objectContaining<ApiError>({
        name: "ApiError",
        status: 409,
        message: "Component set changed",
      }),
    );
  });
});
