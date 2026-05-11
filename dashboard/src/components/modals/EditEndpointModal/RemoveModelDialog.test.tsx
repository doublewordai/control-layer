import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { RemoveModelDialog } from "./RemoveModelDialog";
import { emptyReferences } from "./references";

describe("RemoveModelDialog", () => {
  it("does not render when modelName is null", () => {
    render(
      <RemoveModelDialog
        modelName={null}
        references={emptyReferences()}
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(screen.queryByText(/Remove/)).not.toBeInTheDocument();
  });

  it("renders the model name in the title", () => {
    render(
      <RemoveModelDialog
        modelName="llama-3.1-70b"
        references={emptyReferences()}
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(screen.getByText(/Remove llama-3\.1-70b\?/)).toBeInTheDocument();
  });

  it("lists hosted-model references with the right pluralization", () => {
    render(
      <RemoveModelDialog
        modelName="llama-3.1-70b"
        references={{
          directHosted: [{ modelId: "m1", modelAlias: "fast-llama" }],
          virtualModels: [
            { modelId: "v1", modelAlias: "smart-virtual" },
            { modelId: "v2", modelAlias: "another-virtual" },
          ],
          trafficRules: [],
        }}
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );

    expect(screen.getByText(/1 hosted model wraps this deployment/i)).toBeInTheDocument();
    expect(
      screen.getByText(/2 virtual models include this as a component/i),
    ).toBeInTheDocument();
    expect(screen.getByText("fast-llama")).toBeInTheDocument();
    expect(screen.getByText("smart-virtual")).toBeInTheDocument();
    expect(screen.getByText("another-virtual")).toBeInTheDocument();
  });

  it("lists traffic-rule references with the rule's purpose", () => {
    render(
      <RemoveModelDialog
        modelName="llama-3.1-70b"
        references={{
          directHosted: [],
          virtualModels: [],
          trafficRules: [
            {
              modelId: "v1",
              modelAlias: "tier-1-fallback",
              rule: { api_key_purpose: "batch", action: { type: "redirect", target: "fast-llama" } },
            },
          ],
        }}
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );

    expect(
      screen.getByText(/1 traffic rule redirects to this deployment/i),
    ).toBeInTheDocument();
    expect(screen.getByText("tier-1-fallback")).toBeInTheDocument();
    expect(screen.getByText(/batch purpose/i)).toBeInTheDocument();
  });

  it("calls onConfirm when Remove anyway is clicked", () => {
    const onConfirm = vi.fn();
    render(
      <RemoveModelDialog
        modelName="llama-3.1-70b"
        references={emptyReferences()}
        onConfirm={onConfirm}
        onCancel={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Remove anyway/i }));
    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it("calls onCancel when Cancel is clicked", () => {
    const onCancel = vi.fn();
    render(
      <RemoveModelDialog
        modelName="llama-3.1-70b"
        references={emptyReferences()}
        onConfirm={vi.fn()}
        onCancel={onCancel}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /^Cancel$/i }));
    expect(onCancel).toHaveBeenCalledOnce();
  });
});
