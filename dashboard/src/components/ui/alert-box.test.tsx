import { render, within } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { AlertBox } from "./alert-box";
import { Star } from "lucide-react";

describe("AlertBox", () => {
  describe("rendering", () => {
    it("renders with default info variant", () => {
      const { container } = render(
        <AlertBox>This is an info message</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toBeInTheDocument();
      expect(alert).toHaveTextContent("This is an info message");
    });

    it("renders error variant with correct styling", () => {
      const { container } = render(
        <AlertBox variant="error">Error message</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveClass("bg-red-50", "border-red-200");
      expect(alert).toHaveTextContent("Error message");
    });

    it("renders success variant with correct styling", () => {
      const { container } = render(
        <AlertBox variant="success">Success message</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveClass("bg-green-50", "border-green-200");
      expect(alert).toHaveTextContent("Success message");
    });

    it("renders info variant with correct styling", () => {
      const { container } = render(
        <AlertBox variant="info">Info message</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveClass("bg-blue-50", "border-blue-200");
      expect(alert).toHaveTextContent("Info message");
    });

    it("renders warning variant with correct styling", () => {
      const { container } = render(
        <AlertBox variant="warning">Warning message</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveClass("bg-yellow-50", "border-yellow-200");
      expect(alert).toHaveTextContent("Warning message");
    });
  });

  describe("Icons", () => {
    it("renders default error icon (AlertCircle)", () => {
      const { container } = render(<AlertBox variant="error">Error</AlertBox>);

      // AlertCircle icon should be present (lucide uses SVG)
      const svg = within(container).getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders default success icon (CheckCircle)", () => {
      const { container } = render(
        <AlertBox variant="success">Success</AlertBox>,
      );

      const svg = within(container).getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders default info icon (Info)", () => {
      const { container } = render(<AlertBox variant="info">Info</AlertBox>);

      const svg = within(container).getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders default warning icon (AlertTriangle)", () => {
      const { container } = render(
        <AlertBox variant="warning">Warning</AlertBox>,
      );

      const svg = within(container).getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("render custom icon when provided", () => {
      const { container } = render(
        <AlertBox
          variant="error"
          icon={<Star className="custom-icon" data-testid="custom-icon" />}
        >
          Custom icon alert
        </AlertBox>,
      );

      expect(within(container).getByTestId("custom-icon")).toBeInTheDocument();
      expect(within(container).getByTestId("custom-icon")).toHaveClass(
        "custom-icon",
      );
    });

    it("render no icon when icon prop is null", () => {
      const { container } = render(
        <AlertBox variant="error" icon={null}>
          No icon alert
        </AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      const svg = alert.querySelector("svg");
      expect(svg).not.toBeInTheDocument();
    });
  });

  describe("Content", () => {
    it("render string children", () => {
      const { container } = render(<AlertBox>Simple text message</AlertBox>);

      expect(
        within(container).getByText("Simple text message"),
      ).toBeInTheDocument();
    });

    it("render React node children", () => {
      const { container } = render(
        <AlertBox>
          <div>
            <strong>Bold text</strong> and <em>italic text</em>
          </div>
        </AlertBox>,
      );

      expect(
        within(container).getByText("Bold text", { exact: false }),
      ).toBeInTheDocument();
      expect(
        within(container).getByText("italic text", { exact: false }),
      ).toBeInTheDocument();
    });

    it("render complex nested content", () => {
      const { container } = render(
        <AlertBox variant="warning">
          <div>
            <h4>Warning Title</h4>
            <p>This is a warning description with details.</p>
            <ul>
              <li>Item 1</li>
              <li>Item 2</li>
            </ul>
          </div>
        </AlertBox>,
      );

      expect(within(container).getByText("Warning Title")).toBeInTheDocument();
      expect(
        within(container).getByText(
          "This is a warning description with details.",
        ),
      ).toBeInTheDocument();
      expect(within(container).getByText("Item 1")).toBeInTheDocument();
      expect(within(container).getByText("Item 2")).toBeInTheDocument();
    });
  });

  describe("Styling", () => {
    it("applies custom className", () => {
      const { container } = render(
        <AlertBox className="custom-class mb-8">Message</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveClass("custom-class", "mb-8");
    });

    it("merges custom className with default classes", () => {
      const { container } = render(
        <AlertBox variant="error" className="mt-4">
          Message
        </AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveClass("mt-4", "bg-red-50", "border-red-200");
    });

    it("has proper accessibility role", () => {
      const { container } = render(<AlertBox>Accessible alert</AlertBox>);

      const alert = within(container).getByRole("alert");
      expect(alert).toHaveAttribute("role", "alert");
    });

    it("applies base styling classes to all variants", () => {
      const { container: container1 } = render(
        <AlertBox variant="error">Error</AlertBox>,
      );

      let alert = within(container1).getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");

      const { container: container2 } = render(
        <AlertBox variant="success">Success</AlertBox>,
      );
      alert = within(container2).getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");

      const { container: container3 } = render(
        <AlertBox variant="info">Info</AlertBox>,
      );
      alert = within(container3).getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");

      const { container: container4 } = render(
        <AlertBox variant="warning">Warning</AlertBox>,
      );
      alert = within(container4).getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");
    });
  });

  describe("Layout", () => {
    it("uses flexbox layout with icon and content", () => {
      const { container } = render(
        <AlertBox variant="error">Content</AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      const flexContainer = alert.querySelector(".flex");
      expect(flexContainer).toBeInTheDocument();
      expect(flexContainer).toHaveClass("items-start", "gap-2");
    });

    it("render icon and text in correct order", () => {
      const { container } = render(
        <AlertBox variant="error">
          <span data-testid="content">Error content</span>
        </AlertBox>,
      );

      const alert = within(container).getByRole("alert");
      const flexContainer = alert.querySelector(".flex");
      const children = flexContainer?.children;

      expect(children).toHaveLength(2);
      // First child should be the icon (SVG)
      expect(children?.[0]?.tagName).toBe("svg");
      // Second child should be the content wrapper
      expect(children?.[1]).toContainElement(
        within(container).getByTestId("content"),
      );
    });
  });

  describe("Use Cases", () => {
    it("works as form error display", () => {
      const { container } = render(
        <AlertBox variant="error">
          Please correct the following errors:
          <ul className="mt-2 list-disc list-inside">
            <li>Email is required</li>
            <li>Password must be at least 8 characters</li>
          </ul>
        </AlertBox>,
      );

      expect(
        within(container).getByText("Please correct the following errors:"),
      ).toBeInTheDocument();
      expect(
        within(container).getByText("Email is required"),
      ).toBeInTheDocument();
      expect(
        within(container).getByText("Password must be at least 8 characters"),
      ).toBeInTheDocument();
    });

    it("works as success notification", () => {
      const { container } = render(
        <AlertBox variant="success">
          Your changes have been saved successfully!
        </AlertBox>,
      );

      expect(
        within(container).getByText(
          "Your changes have been saved successfully!",
        ),
      ).toBeInTheDocument();
    });

    it("works as informational banner", () => {
      const { container } = render(
        <AlertBox variant="info">
          Maintenance scheduled for tonight at 2 AM EST.
        </AlertBox>,
      );

      expect(
        within(container).getByText(
          "Maintenance scheduled for tonight at 2 AM EST.",
        ),
      ).toBeInTheDocument();
    });

    it("works as warning message", () => {
      const { container } = render(
        <AlertBox variant="warning">
          Your session will expire in 5 minutes. Please save your work.
        </AlertBox>,
      );

      expect(
        within(container).getByText(
          "Your session will expire in 5 minutes. Please save your work.",
        ),
      ).toBeInTheDocument();
    });
  });
});
