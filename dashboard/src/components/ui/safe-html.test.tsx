import { render } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { SafeHTML } from "./safe-html";

describe("SafeHTML", () => {
  it("renders allowed markup unchanged", () => {
    const { container } = render(<SafeHTML html="<p>hello <strong>world</strong></p>" />);
    expect(container.querySelector("p")).not.toBeNull();
    expect(container.querySelector("strong")?.textContent).toBe("world");
  });

  it("strips <script> tags", () => {
    const { container } = render(
      <SafeHTML html='<div>ok</div><script>window.__pwn = 1;</script>' />,
    );
    expect(container.querySelector("script")).toBeNull();
    expect(container.textContent).toContain("ok");
  });

  it("strips inline event handlers", () => {
    const { container } = render(
      <SafeHTML html='<img src="x" onerror="alert(1)" alt="" />' />,
    );
    const img = container.querySelector("img");
    expect(img).not.toBeNull();
    expect(img?.getAttribute("onerror")).toBeNull();
  });

  it("strips javascript: URLs", () => {
    const { container } = render(
      <SafeHTML html='<a href="javascript:alert(1)">click</a>' />,
    );
    const href = container.querySelector("a")?.getAttribute("href") ?? "";
    expect(/^javascript:/i.test(href)).toBe(false);
  });

  it("preserves <style> blocks (banner content uses them)", () => {
    const { container } = render(
      <SafeHTML html='<style>.dw-bb { color: red; }</style><div class="dw-bb">x</div>' />,
    );
    expect(container.querySelector("style")).not.toBeNull();
  });
});
