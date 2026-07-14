import { fireEvent, render, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type {
  ReasoningTranslationConfig,
  ReasoningTranslationOverrides,
} from "../../../api/control-layer/types";
import {
  ReasoningTranslationEditor,
  ReasoningTranslationOverridesEditor,
} from "./ReasoningTranslationEditor";

function EndpointHarness({
  initialValue = null,
  onChange = vi.fn(),
  onValidityChange = vi.fn(),
}: {
  initialValue?: ReasoningTranslationConfig | null;
  onChange?: (value: ReasoningTranslationConfig | null) => void;
  onValidityChange?: (valid: boolean) => void;
}) {
  const [value, setValue] = useState(initialValue);
  return (
    <ReasoningTranslationEditor
      value={value}
      onChange={(next) => {
        setValue(next);
        onChange(next);
      }}
      onValidityChange={onValidityChange}
    />
  );
}

function OverrideHarness({
  initialValue = null,
  onChange = vi.fn(),
}: {
  initialValue?: ReasoningTranslationOverrides | null;
  onChange?: (value: ReasoningTranslationOverrides | null) => void;
}) {
  const [value, setValue] = useState(initialValue);
  return (
    <ReasoningTranslationOverridesEditor
      value={value}
      onChange={(next) => {
        setValue(next);
        onChange(next);
      }}
      endpointDefault={{
        chat_completions: {
          unsupported_efforts: ["none", "minimal", "low", "medium", "high", "xhigh"],
          writes: [{ target_path: "/endpoint", values: { max: "max" } }],
        },
      }}
    />
  );
}

describe("ReasoningTranslationEditor", () => {
  it("emits a native mapping with all seven efforts mapped or rejected", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const { container } = render(<EndpointHarness onChange={onChange} />);
    const editor = within(container);

    await user.click(editor.getByRole("button", { name: "Configure Chat Completions" }));
    await user.selectOptions(editor.getByLabelText("none decision"), "reject");

    expect(onChange).toHaveBeenLastCalledWith({
      chat_completions: {
        unsupported_efforts: ["none"],
        writes: [
          {
            target_path: "/reasoning_effort",
            values: {
              minimal: "minimal",
              low: "low",
              medium: "medium",
              high: "high",
              xhigh: "xhigh",
              max: "max",
            },
          },
        ],
      },
    });
  });

  it("emits and previews both required token-budget writes", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const initialValue: ReasoningTranslationConfig = {
      chat_completions: {
        unsupported_efforts: ["none", "minimal", "medium", "high", "xhigh", "max"],
        writes: [
          { target_path: "/reasoning_effort", values: { low: "low" } },
          { target_path: "/thinking_token_budget", values: { low: 1024 } },
        ],
      },
    };
    const { container } = render(
      <EndpointHarness initialValue={initialValue} onChange={onChange} />,
    );
    const editor = within(container);

    expect(editor.getByText(/"reasoning_effort": "low"/)).toBeInTheDocument();
    expect(editor.getByText(/"thinking_token_budget": 1024/)).toBeInTheDocument();

    await user.clear(editor.getByLabelText("low token budget"));
    await user.type(editor.getByLabelText("low token budget"), "2048");

    const translation = onChange.mock.lastCall?.[0]?.chat_completions;
    expect(translation.writes).toEqual([
      { target_path: "/reasoning_effort", values: { low: "low" } },
      { target_path: "/thinking_token_budget", values: { low: 2048 } },
    ]);
    expect(editor.getByText(/"thinking_token_budget": 2048/)).toBeInTheDocument();
  });

  it("maps binary On, Off, and Reject decisions at the selected path", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const initialValue: ReasoningTranslationConfig = {
      chat_completions: {
        unsupported_efforts: [],
        writes: [
          {
            target_path: "/chat_template_kwargs/thinking",
            values: {
              none: false,
              minimal: true,
              low: true,
              medium: true,
              high: true,
              xhigh: true,
              max: true,
            },
          },
        ],
      },
    };
    const { container } = render(
      <EndpointHarness initialValue={initialValue} onChange={onChange} />,
    );
    const editor = within(container);

    await user.selectOptions(editor.getByLabelText("minimal decision"), "off");
    await user.selectOptions(editor.getByLabelText("max decision"), "reject");

    expect(onChange.mock.lastCall?.[0]?.chat_completions).toEqual({
      unsupported_efforts: ["max"],
      writes: [
        {
          target_path: "/chat_template_kwargs/thinking",
          values: {
            none: false,
            minimal: false,
            low: true,
            medium: true,
            high: true,
            xhigh: true,
          },
        },
      ],
    });
  });

  it("keeps Chat override and Responses inherit independent", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const { container } = render(<OverrideHarness onChange={onChange} />);
    const editor = within(container);

    await user.click(editor.getByRole("button", { name: "Override Chat Completions" }));

    expect(onChange).toHaveBeenLastCalledWith({
      chat_completions: expect.objectContaining({ mode: "override" }),
      responses: { mode: "inherit" },
    });
    await user.click(editor.getByRole("tab", { name: "Responses" }));
    expect(editor.getByRole("button", { name: "Inherit Responses" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
  });

  it("serializes No mapping as disabled and explains pass-through semantics", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const { container } = render(<OverrideHarness onChange={onChange} />);
    const editor = within(container);

    await user.click(editor.getByRole("button", { name: "No mapping Chat Completions" }));

    expect(onChange).toHaveBeenLastCalledWith({
      chat_completions: { mode: "disabled" },
      responses: { mode: "inherit" },
    });
    expect(editor.getByText(/passes the canonical OpenAI field through/i)).toBeInTheDocument();
    expect(editor.getByText(/does not turn reasoning off/i)).toBeInTheDocument();
  });

  it("keeps the last valid custom value when JSON is invalid and reports invalidity", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const onValidityChange = vi.fn();
    const initialValue: ReasoningTranslationConfig = {
      chat_completions: {
        unsupported_efforts: ["none", "minimal", "low", "medium", "high", "xhigh"],
        writes: [{ target_path: "/reasoning_effort", values: { max: "max" } }],
      },
    };
    const { container } = render(
      <EndpointHarness
        initialValue={initialValue}
        onChange={onChange}
        onValidityChange={onValidityChange}
      />,
    );
    const editor = within(container);

    await user.selectOptions(editor.getByLabelText("Strategy"), "custom");
    onChange.mockClear();
    fireEvent.change(editor.getByLabelText("Custom translation JSON"), {
      target: { value: "{invalid" },
    });

    expect(editor.getByText(/enter valid JSON/i)).toBeInTheDocument();
    expect(onValidityChange).toHaveBeenLastCalledWith(false);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("normalizes model state back to null when both surfaces inherit", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const initialValue: ReasoningTranslationOverrides = {
      chat_completions: { mode: "disabled" },
      responses: { mode: "inherit" },
    };
    const { container } = render(
      <OverrideHarness initialValue={initialValue} onChange={onChange} />,
    );
    const editor = within(container);

    await user.click(editor.getByRole("button", { name: "Inherit Chat Completions" }));

    expect(onChange).toHaveBeenLastCalledWith(null);
  });
});
