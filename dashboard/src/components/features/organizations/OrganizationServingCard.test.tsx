import { render, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { OrganizationServingCard } from "./OrganizationServingCard";

vi.mock("@/api/control-layer/hooks", () => ({
  useOrganizationServing: () => ({
    isLoading: false,
    isError: false,
    data: {
      organization_id: "org",
      granted_serving_classes: [],
      self_hosted_only: false,
      overlays: [],
      cache_tariffs: [],
      model_aliases: { first: "example-first", second: "example-second" },
      tariffs: [
        { deployed_model_id: "first" },
        { deployed_model_id: "second" },
      ],
    },
  }),
  useModel: () => {
    throw new Error("Model names must not trigger individual requests");
  },
}));

describe("organisation token-only overrides", () => {
  it("renders model aliases from the single serving response", () => {
    const { container } = render(
      <OrganizationServingCard organizationId="org" />,
    );
    const page = within(container);
    expect(page.getByText("example-first")).toBeInTheDocument();
    expect(page.getByText("example-second")).toBeInTheDocument();
    expect(
      page.getAllByText("1 token price overrides · 0 cache overrides"),
    ).toHaveLength(2);
  });
});
