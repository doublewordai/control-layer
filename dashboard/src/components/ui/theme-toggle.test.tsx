import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ThemeProvider } from "next-themes";
import { beforeEach, describe, expect, it } from "vitest";
import { ThemeToggle } from "./theme-toggle";

/** jsdom has no matchMedia; next-themes reads it to resolve "system". */
function setSystemDark(dark: boolean) {
  window.matchMedia = ((query: string) => ({
    matches: dark && query.includes("dark"),
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    onchange: null,
    dispatchEvent: () => false,
  })) as typeof window.matchMedia;
}

describe("ThemeToggle", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.className = "";
    setSystemDark(false);
  });

  async function choose(label: string) {
    const user = userEvent.setup();
    render(
      <ThemeProvider attribute="class" defaultTheme="system" enableSystem>
        <ThemeToggle />
      </ThemeProvider>,
    );
    await user.click(screen.getByRole("button", { name: "Change theme" }));
    await user.click(await screen.findByRole("menuitemradio", { name: label }));
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

  it("follows the OS when System is chosen", async () => {
    setSystemDark(true);
    await choose("System");
    expect(localStorage.getItem("theme")).toBe("system");
    expect(document.documentElement).toHaveClass("dark");
  });

  it("marks the active theme as checked", async () => {
    localStorage.setItem("theme", "dark");
    const user = userEvent.setup();
    render(
      <ThemeProvider attribute="class" defaultTheme="system" enableSystem>
        <ThemeToggle />
      </ThemeProvider>,
    );
    await user.click(screen.getByRole("button", { name: "Change theme" }));
    expect(
      await screen.findByRole("menuitemradio", { name: "Dark" }),
    ).toBeChecked();
    expect(screen.getByRole("menuitemradio", { name: "Light" })).not.toBeChecked();
  });
});
