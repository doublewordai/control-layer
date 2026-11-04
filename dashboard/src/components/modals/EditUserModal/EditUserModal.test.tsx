import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { EditUserModal } from "./EditUserModal";

const { mutateAsync } = vi.hoisted(() => ({ mutateAsync: vi.fn() }));
vi.mock("../../../api/control-layer", () => ({
  useUpdateUser: () => ({ mutateAsync, isPending: false }),
  useConfig: () => ({
    data: {
      scheduling_toleration_presets: [],
      onwards: { strict_mode: false },
    },
  }),
}));

describe("EditUserModal serving payload permissions", () => {
  beforeEach(() => {
    mutateAsync.mockReset().mockResolvedValue({});
  });

  it.each([false, true])(
    "only sends serving fields when permitted: %s",
    async (canEditServing) => {
      const user = userEvent.setup();
      render(
        <EditUserModal
          isOpen
          onClose={vi.fn()}
          onSuccess={vi.fn()}
          userId="user-test"
          canEditServing={canEditServing}
          currentUser={{
            name: "Test",
            email: "test@example.com",
            username: "test",
            roles: ["StandardUser"],
            zero_data_retention: false,
            self_hosted_only: true,
          }}
        />,
      );
      await user.click(screen.getByRole("button", { name: "Save Changes" }));
      await waitFor(() => expect(mutateAsync).toHaveBeenCalledOnce());
      const data = mutateAsync.mock.calls[0][0].data;
      if (canEditServing) {
        expect(data).toMatchObject({ self_hosted_only: true });
        expect(data).not.toHaveProperty("granted_serving_classes");
        expect(data).not.toHaveProperty("default_serving_class");
      } else {
        for (const field of ["self_hosted_only", "toleration_preset"]) {
          expect(data).not.toHaveProperty(field);
        }
      }
    },
  );
});

describe("EditUserModal layout", () => {
  it("scrolls the form body while the actions stay outside it", () => {
    render(
      <EditUserModal
        isOpen
        onClose={vi.fn()}
        onSuccess={vi.fn()}
        userId="user-test"
        canEditServing
        canEditZdr
        currentUser={{
          name: "Test",
          email: "test@example.com",
          username: "test",
          roles: ["StandardUser"],
          zero_data_retention: false,
        }}
      />,
    );
    const dialog = screen.getByRole("dialog");
    expect(dialog.className).toContain("max-h-[90vh]");
    const body = within(dialog).getByTestId("edit-user-form-body");
    expect(body.className).toContain("overflow-y-auto");
    const save = within(dialog).getByRole("button", { name: "Save Changes" });
    expect(body.contains(save)).toBe(false);
  });
});

describe("EditUserModal support permissions", () => {
  it("enables ZDR without submitting roles or other account fields", async () => {
    mutateAsync.mockReset().mockResolvedValue({});
    const user = userEvent.setup();
    render(<EditUserModal isOpen supportOnly canEditZdr onClose={vi.fn()} onSuccess={vi.fn()}
      userId="customer-test" currentUser={{ name: "Customer", email: "customer@example.com", username: "customer",
        roles: ["StandardUser"], zero_data_retention: false }} />);
    const dialog = within(screen.getByRole("dialog"));
    expect(dialog.queryByRole("checkbox", { name: /Platform Manager/ })).not.toBeInTheDocument();
    await user.click(dialog.getByRole("switch", { name: "Toggle zero data retention" }));
    await user.click(dialog.getByRole("button", { name: "Save Changes" }));
    await waitFor(() => expect(mutateAsync).toHaveBeenCalledWith({ id: "customer-test", data: { zero_data_retention: true } }));
  });

  it("cannot disable ZDR for another account", () => {
    render(<EditUserModal isOpen supportOnly canEditZdr onClose={vi.fn()} onSuccess={vi.fn()}
      userId="customer-test" currentUser={{ name: "Customer", email: "customer@example.com", username: "customer",
        roles: ["StandardUser"], zero_data_retention: true }} />);
    const dialog = within(screen.getByRole("dialog"));
    expect(dialog.getByRole("switch", { name: "Toggle zero data retention" })).toBeDisabled();
    expect(dialog.getByRole("button", { name: "Save Changes" })).toBeDisabled();
  });
});
