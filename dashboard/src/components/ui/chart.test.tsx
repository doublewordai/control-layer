import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ChartStyle } from "./chart";

describe("ChartStyle", () => {
  it("renders safe chart variables as style text", () => {
    const { container } = render(
      <ChartStyle
        id="chart-example"
        config={{ requests: { color: "var(--chart-1)" } }}
      />,
    );

    expect(container.querySelector("style")?.textContent).toContain(
      '[data-chart="chart-example"]',
    );
    expect(container.querySelector("style")?.textContent).toContain(
      "--color-requests: var(--chart-1);",
    );
  });

  it("does not render unsafe CSS identifiers or values", () => {
    const { container } = render(
      <ChartStyle
        id="chart-example"
        config={{
          'requests} body': { color: "red" },
          requests: { color: "red; } body { background: url(/leak)" },
        }}
      />,
    );

    expect(container.querySelector("style")).toBeNull();
    expect(container.querySelector("script")).toBeNull();
  });

  it("does not render an unsafe chart identifier", () => {
    const { container } = render(
      <ChartStyle
        id={'chart-example"] {} body {'}
        config={{ requests: { color: "red" } }}
      />,
    );

    expect(container.querySelector("style")).toBeNull();
  });
});
