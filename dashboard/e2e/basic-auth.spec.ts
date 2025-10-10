import { test, expect } from "@playwright/test";
import { AuthHelper } from "./auth.helper";

// Get admin email from environment variable, with fallback for backward compatibility
const adminEmail = process.env.ADMIN_EMAIL || "test@doubleword.ai";

test.describe("Authentication Flow", () => {
  test("should redirect unauthenticated users to login", async ({ page }) => {
    // Navigate to the dashboard without authentication
    await page.goto("http://localhost:3001/");

    // Should be redirected to login page
    await expect(page).toHaveURL(/\/login$/);

    // Should see login form
    await expect(page.getByText(/sign in/i).first()).toBeVisible();
  });

  test("admin should have full dashboard access", async ({ page }) => {
    const auth = new AuthHelper(page);

    // Login as admin
    await auth.loginAsAdmin();
    await page.goto("/");

    // Should reach the dashboard
    await expect(page).toHaveURL(/^http:\/\/localhost:3001\/(models)?$/);

    // Should see admin user info in the sidebar
    await expect(
      page.getByRole("button").filter({ hasText: adminEmail }),
    ).toBeVisible();

    // Should see all navigation options including admin-only ones
    await expect(page.getByRole("link", { name: /models/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /playground/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /api keys/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /settings/i })).toBeVisible();

    // Admin-only navigation items
    await expect(
      page.getByRole("link", { name: /users.*groups/i }),
    ).toBeVisible();
  
    await expect(page.getByRole("link", { name: /endpoints/i })).toBeVisible();
  });

  test("regular user should have limited dashboard access", async ({
    page,
  }) => {
    const auth = new AuthHelper(page);

    // Login as regular user
    await auth.loginAsUser();
    await page.goto("/");

    // Should reach the dashboard
    await expect(page).toHaveURL(/^http:\/\/localhost:3001\/(models)?$/);

    // Should see user info in the sidebar
    await expect(
      page.getByRole("button").filter({ hasText: "user@example.org" }),
    ).toBeVisible();

    // Should see user-accessible navigation options
    await expect(page.getByRole("link", { name: /models/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /playground/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /api keys/i })).toBeVisible();

    // Should NOT see admin-only navigation items
    await expect(
      page.getByRole("link", { name: /settings/i }),
    ).not.toBeVisible();
    await expect(
      page.getByRole("link", { name: /users.*groups/i }),
    ).not.toBeVisible();
    await expect(
      page.getByRole("link", { name: /endpoints/i }),
    ).not.toBeVisible();
  });
});
