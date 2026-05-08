import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { AddModelPalette } from "./AddModelPalette";
import type { AvailableModel } from "../../../api/control-layer/types";

const sampleCatalog: AvailableModel[] = [
  { id: "llama-3.1-70b-instruct", created: 0, object: "model", owned_by: "meta" },
  { id: "qwen-2.5-72b-instruct", created: 0, object: "model", owned_by: "alibaba" },
  { id: "qwen-coder-32b", created: 0, object: "model", owned_by: "alibaba" },
  { id: "deepseek-v3", created: 0, object: "model", owned_by: "deepseek" },
];

describe("AddModelPalette", () => {
  it("renders an Add model trigger button", () => {
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set()}
        onAdd={vi.fn()}
      />,
    );
    expect(screen.getByRole("button", { name: /Add model/i })).toBeInTheDocument();
  });

  it("opens the palette and shows catalog entries", async () => {
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set()}
        onAdd={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));

    await waitFor(() => {
      expect(screen.getByText("llama-3.1-70b-instruct")).toBeInTheDocument();
      expect(screen.getByText("deepseek-v3")).toBeInTheDocument();
    });
  });

  it("filters catalog entries by typed query", async () => {
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set()}
        onAdd={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
    const input = await waitFor(() =>
      screen.getByPlaceholderText(/Search 4 models/i),
    );

    fireEvent.change(input, { target: { value: "qwen" } });

    await waitFor(() => {
      expect(screen.getByText("qwen-2.5-72b-instruct")).toBeInTheDocument();
      expect(screen.getByText("qwen-coder-32b")).toBeInTheDocument();
      expect(screen.queryByText("deepseek-v3")).not.toBeInTheDocument();
    });
  });

  it("shows a manual-add option when the typed query has no catalog match", async () => {
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set()}
        onAdd={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
    const input = await waitFor(() =>
      screen.getByPlaceholderText(/Search 4 models/i),
    );

    fireEvent.change(input, { target: { value: "my-finetune-v2" } });

    await waitFor(() => {
      expect(screen.getByText(/Add manually:/i)).toBeInTheDocument();
      expect(screen.getByText("my-finetune-v2")).toBeInTheDocument();
    });
  });

  it("calls onAdd with the catalog model id when a catalog entry is picked", async () => {
    const onAdd = vi.fn();
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set()}
        onAdd={onAdd}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
    const entry = await waitFor(() => screen.getByText("deepseek-v3"));
    fireEvent.click(entry);

    expect(onAdd).toHaveBeenCalledWith("deepseek-v3");
  });

  it("calls onAdd with the typed query when manual-add is picked", async () => {
    const onAdd = vi.fn();
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set()}
        onAdd={onAdd}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
    const input = await waitFor(() =>
      screen.getByPlaceholderText(/Search 4 models/i),
    );

    fireEvent.change(input, { target: { value: "custom-finetune" } });

    const manualRow = await waitFor(() => screen.getByText(/Add manually:/i));
    fireEvent.click(manualRow);

    expect(onAdd).toHaveBeenCalledWith("custom-finetune");
  });

  it("works with an empty catalog by accepting only manual entries", async () => {
    const onAdd = vi.fn();
    render(
      <AddModelPalette catalog={[]} importedModelNames={new Set()} onAdd={onAdd} />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
    const input = await waitFor(() =>
      screen.getByPlaceholderText(/Type a model name/i),
    );

    fireEvent.change(input, { target: { value: "manual-only" } });

    const manualRow = await waitFor(() => screen.getByText(/Add manually:/i));
    fireEvent.click(manualRow);

    expect(onAdd).toHaveBeenCalledWith("manual-only");
  });

  it("does not show the manual-add row when the typed name matches an already-imported model", async () => {
    render(
      <AddModelPalette
        catalog={sampleCatalog}
        importedModelNames={new Set(["deepseek-v3"])}
        onAdd={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
    const input = await waitFor(() =>
      screen.getByPlaceholderText(/Search 4 models/i),
    );

    fireEvent.change(input, { target: { value: "deepseek-v3" } });

    // The "already imported" group appears, manual-add does not
    await waitFor(() => {
      // The group heading + per-row label both contain this string,
      // so we expect at least one match and check no manual-add row.
      expect(screen.getAllByText(/already imported/i).length).toBeGreaterThan(0);
      expect(screen.queryByText(/Add manually:/i)).not.toBeInTheDocument();
    });
  });
});
