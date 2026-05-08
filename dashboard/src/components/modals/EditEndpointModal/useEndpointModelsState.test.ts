import { describe, it, expect } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useEndpointModelsState } from "./useEndpointModelsState";

const initial = [
  { modelName: "llama-70b", alias: "fast-llama" },
  { modelName: "qwen-32b", alias: "qwen" },
];

describe("useEndpointModelsState", () => {
  it("starts in a clean state with the server deployments visible", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    expect(result.current.hasChanges).toBe(false);
    expect(result.current.changeCount).toBe(0);
    expect(result.current.deployments).toEqual([
      { modelName: "llama-70b", alias: "fast-llama", isNew: false },
      { modelName: "qwen-32b", alias: "qwen", isNew: false },
    ]);
  });

  it("adds a new model with a default alias", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.addModel("deepseek-v3"));

    expect(result.current.deployments).toContainEqual({
      modelName: "deepseek-v3",
      alias: "deepseek-v3",
      isNew: true,
    });
    expect(result.current.addedModelNames).toEqual(["deepseek-v3"]);
    expect(result.current.changeCount).toBe(1);
  });

  it("does not add a model already present on the server", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.addModel("llama-70b"));

    expect(result.current.addedModelNames).toEqual([]);
    expect(result.current.hasChanges).toBe(false);
  });

  it("does not double-add the same staged model", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.addModel("deepseek-v3"));
    act(() => result.current.addModel("deepseek-v3"));

    expect(result.current.addedModelNames).toEqual(["deepseek-v3"]);
  });

  it("re-adding a previously-removed server deployment cancels the removal", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.removeModel("llama-70b"));
    expect(result.current.removedModelNames).toEqual(["llama-70b"]);

    act(() => result.current.addModel("llama-70b"));
    expect(result.current.removedModelNames).toEqual([]);
    expect(result.current.deployments.find((d) => d.modelName === "llama-70b")).toBeDefined();
  });

  it("removing a server deployment hides it and stages a removal", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.removeModel("llama-70b"));

    expect(result.current.deployments.map((d) => d.modelName)).toEqual(["qwen-32b"]);
    expect(result.current.removedModelNames).toEqual(["llama-70b"]);
    expect(result.current.changeCount).toBe(1);
  });

  it("removing a freshly-added model just drops it", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.addModel("deepseek-v3"));
    act(() => result.current.removeModel("deepseek-v3"));

    expect(result.current.addedModelNames).toEqual([]);
    expect(result.current.removedModelNames).toEqual([]);
    expect(result.current.hasChanges).toBe(false);
  });

  it("undoRemove restores a removed server deployment", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.removeModel("llama-70b"));
    act(() => result.current.undoRemove("llama-70b"));

    expect(result.current.removedModelNames).toEqual([]);
    expect(result.current.deployments.find((d) => d.modelName === "llama-70b")).toBeDefined();
  });

  it("setAlias updates a server deployment alias and counts as a change", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.setAlias("llama-70b", "llama-renamed"));

    expect(result.current.deployments.find((d) => d.modelName === "llama-70b")?.alias).toBe(
      "llama-renamed",
    );
    expect(result.current.changeCount).toBe(1);
  });

  it("setting an alias back to the server value clears the change", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.setAlias("llama-70b", "llama-renamed"));
    expect(result.current.changeCount).toBe(1);

    act(() => result.current.setAlias("llama-70b", "fast-llama"));
    expect(result.current.changeCount).toBe(0);
  });

  it("setAlias on a newly-added model updates the added entry directly", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => result.current.addModel("deepseek-v3"));
    act(() => result.current.setAlias("deepseek-v3", "deep"));

    const added = result.current.deployments.find((d) => d.modelName === "deepseek-v3");
    expect(added?.alias).toBe("deep");
    // alias edit on a new model is part of the add, not a separate change
    expect(result.current.changeCount).toBe(1);
  });

  it("reset clears all staged changes", () => {
    const { result } = renderHook(() => useEndpointModelsState(initial));

    act(() => {
      result.current.addModel("deepseek-v3");
      result.current.removeModel("llama-70b");
      result.current.setAlias("qwen-32b", "renamed");
    });
    expect(result.current.changeCount).toBeGreaterThan(0);

    act(() => result.current.reset());

    expect(result.current.changeCount).toBe(0);
    expect(result.current.deployments).toEqual([
      { modelName: "llama-70b", alias: "fast-llama", isNew: false },
      { modelName: "qwen-32b", alias: "qwen", isNew: false },
    ]);
  });
});
