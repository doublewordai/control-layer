import { render, within, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll, vi } from "vitest";
import userEvent from "@testing-library/user-event";
import { AutoTopupSection } from "./AutoTopupSection";
import type { User } from "@/api/control-layer";

function makeUser(overrides: Partial<User> = {}): User {
  return {
    id: "user-1",
    username: "testuser",
    external_user_id: "ext-1",
    email: "test@example.com",
    roles: ["StandardUser"],
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
    auth_source: "native",
    has_payment_provider_id: true,
    batch_notifications_enabled: false,
    low_balance_threshold: null,
    auto_topup_amount: null,
    auto_topup_threshold: null,
    has_auto_topup_payment_method: false,
    auto_topup_monthly_limit: null,
    ...overrides,
  } as User;
}

const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("AutoTopupSection", () => {
  it("renders toggle in off state when auto-topup is disabled", () => {
    const { container } = render(
      <AutoTopupSection user={makeUser()} onSuccess={vi.fn()} />,
      { wrapper: createWrapper() },
    );

    const toggle = within(container).getByRole("switch", { name: "Toggle auto top-up" });
    expect(toggle).not.toBeChecked();
  });

  it("renders toggle in on state when auto-topup is enabled", () => {
    const { container } = render(
      <AutoTopupSection
        user={makeUser({
          auto_topup_amount: 25,
          auto_topup_threshold: 5,
          has_auto_topup_payment_method: true,
        })}
        onSuccess={vi.fn()}
      />,
      { wrapper: createWrapper() },
    );

    const toggle = within(container).getByRole("switch", { name: "Toggle auto top-up" });
    expect(toggle).toBeChecked();
  });

  it("calls enable endpoint (not user patch) when toggling on", async () => {
    const enableHandler = vi.fn();
    server.use(
      http.post("/admin/api/v1/auto-topup/enable", async ({ request }) => {
        enableHandler(await request.json());
        return HttpResponse.json({ has_payment_method: true, threshold: 5, amount: 25 });
      }),
    );

    const onSuccess = vi.fn();
    const { container } = render(
      <AutoTopupSection user={makeUser()} onSuccess={onSuccess} />,
      { wrapper: createWrapper() },
    );

    const user = userEvent.setup();
    const toggle = within(container).getByRole("switch", { name: "Toggle auto top-up" });
    await user.click(toggle);

    await waitFor(() => {
      expect(enableHandler).toHaveBeenCalledWith(
        expect.objectContaining({ threshold: 5, amount: 25 }),
      );
    });

    await waitFor(() => {
      expect(onSuccess).toHaveBeenCalled();
    });
  });

  it("calls disable endpoint (not user patch) when toggling off", async () => {
    const disableHandler = vi.fn();
    server.use(
      http.post("/admin/api/v1/auto-topup/disable", () => {
        disableHandler();
        return HttpResponse.json({ message: "Auto top-up disabled" });
      }),
    );

    const onSuccess = vi.fn();
    const { container } = render(
      <AutoTopupSection
        user={makeUser({
          auto_topup_amount: 25,
          auto_topup_threshold: 5,
          has_auto_topup_payment_method: true,
        })}
        onSuccess={onSuccess}
      />,
      { wrapper: createWrapper() },
    );

    const user = userEvent.setup();
    const toggle = within(container).getByRole("switch", { name: "Toggle auto top-up" });
    await user.click(toggle);

    await waitFor(() => {
      expect(disableHandler).toHaveBeenCalled();
    });

    await waitFor(() => {
      expect(onSuccess).toHaveBeenCalled();
    });
  });

  it("does not call user patch endpoint for any operation", async () => {
    const patchHandler = vi.fn();
    server.use(
      http.patch("/admin/api/v1/users/:id", () => {
        patchHandler();
        return HttpResponse.json({});
      }),
      http.post("/admin/api/v1/auto-topup/enable", () => {
        return HttpResponse.json({ has_payment_method: true, threshold: 5, amount: 25 });
      }),
      http.post("/admin/api/v1/auto-topup/disable", () => {
        return HttpResponse.json({ message: "Auto top-up disabled" });
      }),
    );

    // Toggle on
    const { container, rerender } = render(
      <AutoTopupSection user={makeUser()} onSuccess={vi.fn()} />,
      { wrapper: createWrapper() },
    );

    const user = userEvent.setup();
    await user.click(within(container).getByRole("switch", { name: "Toggle auto top-up" }));

    await waitFor(() => {
      expect(patchHandler).not.toHaveBeenCalled();
    });

    // Toggle off
    rerender(
      <AutoTopupSection
        user={makeUser({
          auto_topup_amount: 25,
          auto_topup_threshold: 5,
          has_auto_topup_payment_method: true,
        })}
        onSuccess={vi.fn()}
      />,
    );

    await user.click(within(container).getByRole("switch", { name: "Toggle auto top-up" }));

    await waitFor(() => {
      expect(patchHandler).not.toHaveBeenCalled();
    });
  });
});
