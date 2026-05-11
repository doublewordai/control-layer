import { describe, it, expect, beforeEach } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { ReactNode } from "react";
import { MemoryRouter, useSearchParams } from "react-router-dom";
import {
  usePersistedFilter,
  clearPersistedFilters,
} from "./usePersistedFilter";

function wrapperWithRouter(initialEntries: string[] = ["/"]) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>;
  };
}

describe("usePersistedFilter", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("returns the fallback when nothing is persisted and URL is empty", () => {
    const { result } = renderHook(
      () => usePersistedFilter("page-a", "status", "all"),
      { wrapper: wrapperWithRouter() },
    );
    expect(result.current[0]).toBe("all");
  });

  it("uses URL value when present, even if localStorage has a different default", () => {
    localStorage.setItem(
      "filters:page-a",
      JSON.stringify({ status: "completed" }),
    );
    const { result } = renderHook(
      () => usePersistedFilter("page-a", "status", "all"),
      { wrapper: wrapperWithRouter(["/?status=failed"]) },
    );
    expect(result.current[0]).toBe("failed");
  });

  it("falls back to localStorage when URL is empty", () => {
    localStorage.setItem(
      "filters:page-a",
      JSON.stringify({ status: "completed" }),
    );
    const { result } = renderHook(
      () => usePersistedFilter("page-a", "status", "all"),
      { wrapper: wrapperWithRouter() },
    );
    expect(result.current[0]).toBe("completed");
  });

  it("writes new values to both URL params and localStorage", () => {
    const { result } = renderHook(
      () => {
        const [value, setValue] = usePersistedFilter("page-a", "status", "all");
        const [params] = useSearchParams();
        return { value, setValue, params };
      },
      { wrapper: wrapperWithRouter() },
    );

    act(() => {
      result.current.setValue("completed");
    });

    expect(result.current.value).toBe("completed");
    expect(result.current.params.get("status")).toBe("completed");
    expect(localStorage.getItem("filters:page-a")).toBe(
      JSON.stringify({ status: "completed" }),
    );
  });

  it("removes the URL param and localStorage entry when value matches the fallback", () => {
    localStorage.setItem(
      "filters:page-a",
      JSON.stringify({ status: "completed" }),
    );
    const { result } = renderHook(
      () => {
        const [value, setValue] = usePersistedFilter("page-a", "status", "all");
        const [params] = useSearchParams();
        return { value, setValue, params };
      },
      { wrapper: wrapperWithRouter(["/?status=completed"]) },
    );

    act(() => {
      result.current.setValue("all");
    });

    expect(result.current.value).toBe("all");
    expect(result.current.params.get("status")).toBeNull();
    expect(localStorage.getItem("filters:page-a")).toBeNull();
  });

  it("isolates scopes so different pages don't collide", () => {
    const wrapper = wrapperWithRouter();
    const a = renderHook(
      () => usePersistedFilter("page-a", "status", "all"),
      { wrapper },
    );
    const b = renderHook(
      () => usePersistedFilter("page-b", "status", "all"),
      { wrapper: wrapperWithRouter() },
    );

    act(() => {
      a.result.current[1]("completed");
    });

    expect(localStorage.getItem("filters:page-a")).toBe(
      JSON.stringify({ status: "completed" }),
    );
    expect(localStorage.getItem("filters:page-b")).toBeNull();
    expect(b.result.current[0]).toBe("all");
  });

  it("supports array filter values via comma-separated URL params", () => {
    const empty: string[] = [];
    const { result } = renderHook(
      () => {
        const [value, setValue] = usePersistedFilter("page-a", "models", empty);
        const [params] = useSearchParams();
        return { value, setValue, params };
      },
      { wrapper: wrapperWithRouter() },
    );

    act(() => {
      result.current.setValue(["gpt-4", "claude"]);
    });

    expect(result.current.value).toEqual(["gpt-4", "claude"]);
    expect(result.current.params.get("models")).toBe("gpt-4,claude");
  });

  it("reads array values from URL", () => {
    const empty: string[] = [];
    const { result } = renderHook(
      () => usePersistedFilter("page-a", "models", empty),
      { wrapper: wrapperWithRouter(["/?models=a,b,c"]) },
    );
    expect(result.current[0]).toEqual(["a", "b", "c"]);
  });

  it("supports updater-fn setters and survives back-to-back updates without losing the first one", () => {
    const empty: string[] = [];
    const { result } = renderHook(
      () => usePersistedFilter("page-a", "models", empty),
      { wrapper: wrapperWithRouter() },
    );

    act(() => {
      // Two updates in the same tick — the second one's prev must reflect
      // the first one's write, not the stale closure.
      result.current[1]((prev) => [...prev, "a"]);
      result.current[1]((prev) => [...prev, "b"]);
    });

    expect(result.current[0]).toEqual(["a", "b"]);
    expect(
      JSON.parse(localStorage.getItem("filters:page-a") || "{}").models,
    ).toEqual(["a", "b"]);
  });
});

describe("clearPersistedFilters", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("removes the scope's localStorage entry and the named URL params", () => {
    localStorage.setItem(
      "filters:page-a",
      JSON.stringify({ status: "completed", model: "gpt-4" }),
    );
    localStorage.setItem(
      "filters:page-b",
      JSON.stringify({ status: "failed" }),
    );

    const { result } = renderHook(
      () => {
        const [params, setParams] = useSearchParams();
        return { params, setParams };
      },
      {
        wrapper: wrapperWithRouter(["/?status=completed&model=gpt-4&keep=yes"]),
      },
    );

    act(() => {
      clearPersistedFilters("page-a", result.current.setParams, [
        "status",
        "model",
      ]);
    });

    expect(localStorage.getItem("filters:page-a")).toBeNull();
    expect(localStorage.getItem("filters:page-b")).toBe(
      JSON.stringify({ status: "failed" }),
    );
    expect(result.current.params.get("status")).toBeNull();
    expect(result.current.params.get("model")).toBeNull();
    expect(result.current.params.get("keep")).toBe("yes");
  });

  it("only deletes the named keys within the scope, leaving other keys in the same bucket alone", () => {
    localStorage.setItem(
      "filters:page-a",
      JSON.stringify({
        status: "completed",
        model: "gpt-4",
        unrelated: "keep-me",
      }),
    );

    const { result } = renderHook(
      () => {
        const [params, setParams] = useSearchParams();
        return { params, setParams };
      },
      { wrapper: wrapperWithRouter() },
    );

    act(() => {
      clearPersistedFilters("page-a", result.current.setParams, [
        "status",
        "model",
      ]);
    });

    expect(JSON.parse(localStorage.getItem("filters:page-a") || "{}")).toEqual({
      unrelated: "keep-me",
    });
  });
});
