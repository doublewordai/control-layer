import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { EditUserModal } from "./EditUserModal";

const { mutateAsync } = vi.hoisted(() => ({ mutateAsync: vi.fn() }));
vi.mock("../../../api/control-layer", () => ({
  useUpdateUser: () => ({ mutateAsync, isPending: false }),
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
            granted_serving_classes: ["interactive"],
            default_serving_class: null,
            self_hosted_only: true,
          }}
        />,
      );
      await user.click(screen.getByRole("button", { name: "Save Changes" }));
      await waitFor(() => expect(mutateAsync).toHaveBeenCalledOnce());
      const data = mutateAsync.mock.calls[0][0].data;
      if (canEditServing) {
        expect(data).toMatchObject({
          granted_serving_classes: ["interactive"],
          default_serving_class: null,
          self_hosted_only: true,
        });
      } else {
        for (const field of [
          "granted_serving_classes",
          "default_serving_class",
          "self_hosted_only",
        ]) {
          expect(data).not.toHaveProperty(field);
        }
      }
    },
  );
});
