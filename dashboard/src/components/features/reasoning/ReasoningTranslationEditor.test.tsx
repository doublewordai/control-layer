import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { ReasoningTranslationConfig } from "../../../api/control-layer/types";
import { ReasoningTranslationEditor } from "./ReasoningTranslationEditor";

function Harness({
  initialValue = null,
  allowInherit = false,
  onChange = vi.fn(),
}: {
  initialValue?: ReasoningTranslationConfig | null;
  allowInherit?: boolean;
  onChange?: (value: ReasoningTranslationConfig | null) => void;
}) {
  const [value, setValue] = useState(initialValue);
  return (
    <ReasoningTranslationEditor
      value={value}
      allowInherit={allowInherit}
      onChange={(next) => {
        setValue(next);
        onChange(next);
      }}
    />
  );
}

describe("ReasoningTranslationEditor", () => {
  it("creates the SGLang chat template mapping", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<Harness onChange={onChange} />);

    await user.click(screen.getByRole("button", { name: /use sglang preset/i }));

    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        chat_completions: expect.objectContaining({
          target_path: "/chat_template_kwargs/thinking",
          values: expect.objectContaining({ none: false, low: true }),
        }),
      }),
    );
    expect(screen.getByText(/"chat_template_kwargs"/)).toBeInTheDocument();
    expect(screen.getByText(/"thinking": false/)).toBeInTheDocument();
  });

  it("reports invalid mapped JSON without replacing the valid config", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<Harness onChange={onChange} />);
    await user.click(screen.getByRole("button", { name: /use sglang preset/i }));
    onChange.mockClear();

    const input = screen.getByLabelText("Chat Completions none value");
    fireEvent.change(input, { target: { value: "{invalid" } });

    expect(screen.getByText(/enter valid json/i)).toBeInTheDocument();
    expect(onChange).not.toHaveBeenCalled();
  });

  it("removes a surface instead of emitting an empty effort map", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(
      <Harness
        onChange={onChange}
        initialValue={{
          chat_completions: {
            target_path: "/thinking/type",
            values: { none: "disabled" },
          },
        }}
      />,
    );

    await user.click(screen.getByRole("checkbox", { name: "none" }));

    expect(onChange).toHaveBeenLastCalledWith(null);
    expect(
      screen.getByRole("checkbox", { name: "Configure Chat Completions" }),
    ).not.toBeChecked();
  });

  it("lets a model return to its endpoint default", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(
      <Harness
        allowInherit
        onChange={onChange}
        initialValue={{
          chat_completions: {
            target_path: "/thinking/type",
            values: { none: "disabled" },
          },
        }}
      />,
    );

    await user.click(screen.getByRole("button", { name: /use endpoint default/i }));

    expect(onChange).toHaveBeenLastCalledWith(null);
    expect(screen.getByText(/inheriting the endpoint mapping/i)).toBeInTheDocument();
  });
});
