import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ThemeProvider } from "next-themes";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { ThemeToggle } from "./theme-toggle";

describe("ThemeToggle", () => {
  beforeAll(() => {
    // jsdom has no matchMedia, which next-themes uses for the system option.
    window.matchMedia ??= ((query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      onchange: null,
      dispatchEvent: () => false,
    })) as typeof window.matchMedia;
  });

  beforeEach(() => {
    localStorage.clear();
    document.documentElement.className = "";
  });

  async function choose(label: string) {
    const user = userEvent.setup();
    render(
      <ThemeProvider attribute="class" defaultTheme="light" enableSystem>
        <ThemeToggle />
      </ThemeProvider>,
    );
    await user.click(screen.getByRole("button", { name: "Change theme" }));
    await user.click(await screen.findByRole("menuitem", { name: label }));
  }

  it("adds the dark class to the document when Dark is chosen", async () => {
    await choose("Dark");
    expect(document.documentElement).toHaveClass("dark");
    expect(localStorage.getItem("theme")).toBe("dark");
  });

  it("removes the dark class when Light is chosen", async () => {
    document.documentElement.classList.add("dark");
    localStorage.setItem("theme", "dark");
    await choose("Light");
    expect(document.documentElement).not.toHaveClass("dark");
    expect(localStorage.getItem("theme")).toBe("light");
  });
});
