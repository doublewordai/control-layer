import { Page } from "@playwright/test";
import { execSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * Authentication helper for E2E tests
 * Uses the actual JWT generation script from scripts/generate-jwt.sh
 */
export class AuthHelper {
  private adminEmail: string;

  constructor(private page: Page) {
    // Get admin email from environment variable, with fallback for backward compatibility
    this.adminEmail = process.env.ADMIN_EMAIL || "yicheng@doubleword.ai";
  }

  /**
   * Set authentication cookie using username and password via the login endpoint
   */
  async loginWithCredentials(username: string, password: string) {
    const cookie = this.generateCookie(username, password);

    // Set the clay_session cookie for native authentication
    await this.page.context().addCookies([
      {
        name: "clay_session",
        value: cookie,
        domain: "localhost",
        path: "/",
        secure: true,
        httpOnly: true,
      },
    ]);
  }

  /**
   * Login as admin user
   */
  async loginAsAdmin() {
    const password = process.env.ADMIN_PASSWORD || "admin_password";
    await this.loginWithCredentials(this.adminEmail, password);
  }

  /**
   * Login as regular user
   */
  async loginAsUser() {
    const password = process.env.USER_PASSWORD || "user_password";
    await this.loginWithCredentials("user@example.org", password);
  }

  /**
   * Logout by clearing cookies
   */
  async logout() {
    await this.page.context().clearCookies();
  }

  /**
   * Generate cookie by calling the login endpoint via the script
   */
  private generateCookie(username: string, password: string): string {
    try {
      // Path to the generate-jwt.sh script relative to the dashboard directory
      const scriptPath = path.resolve(
        __dirname,
        "../../../scripts/generate-jwt.sh",
      );

      // Execute the script with username and password
      return execSync(
        `USERNAME="${username}" PASSWORD="${password}" ${scriptPath}`,
        {
          encoding: "utf8",
          stdio: ["pipe", "pipe", "pipe"], // Capture stderr separately to avoid validation output
        },
      ).trim();
    } catch (error) {
      throw new Error(
        `Failed to generate cookie for ${username}: ${error}`,
      );
    }
  }
}
