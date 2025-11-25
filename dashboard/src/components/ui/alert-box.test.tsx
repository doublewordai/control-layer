import { render, screen } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { AlertBox } from "./alert-box";
import { Star } from "lucide-react";

describe("AlertBox", () => {
  describe("Rendering", () => {
    it("renders with default info variant", () => {
      render(<AlertBox>This is an info message</AlertBox>);

      const alert = screen.getByRole("alert");
      expect(alert).toBeInTheDocument();
      expect(alert).toHaveTextContent("This is an info message");
    });

    it("renders error variant with correct styling", () => {
      render(<AlertBox variant="error">Error message</AlertBox>);

      const alert = screen.getByRole("alert");
      expect(alert).toHaveClass("bg-red-50", "border-red-200");
      expect(alert).toHaveTextContent("Error message");
    });

    it("renders success variant with correct styling", () => {
      render(<AlertBox variant="success">Success message</AlertBox>);

      const alert = screen.getByRole("alert");
      expect(alert).toHaveClass("bg-green-50", "border-green-200");
      expect(alert).toHaveTextContent("Success message");
    });

    it("renders info variant with correct styling", () => {
      render(<AlertBox variant="info">Info message</AlertBox>);

      const alert = screen.getByRole("alert");
      expect(alert).toHaveClass("bg-blue-50", "border-blue-200");
      expect(alert).toHaveTextContent("Info message");
    });

    it("renders warning variant with correct styling", () => {
      render(<AlertBox variant="warning">Warning message</AlertBox>);

      const alert = screen.getByRole("alert");
      expect(alert).toHaveClass("bg-yellow-50", "border-yellow-200");
      expect(alert).toHaveTextContent("Warning message");
    });
  });

  describe("Icons", () => {
    it("renders default error icon (AlertCircle)", () => {
      render(<AlertBox variant="error">Error</AlertBox>);

      // AlertCircle icon should be present (lucide uses SVG)
      const svg = screen.getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders default success icon (CheckCircle)", () => {
      render(<AlertBox variant="success">Success</AlertBox>);

      const svg = screen.getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders default info icon (Info)", () => {
      render(<AlertBox variant="info">Info</AlertBox>);

      const svg = screen.getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders default warning icon (AlertTriangle)", () => {
      render(<AlertBox variant="warning">Warning</AlertBox>);

      const svg = screen.getByRole("alert").querySelector("svg");
      expect(svg).toBeInTheDocument();
    });

    it("renders custom icon when provided", () => {
      render(
        <AlertBox
          variant="error"
          icon={<Star className="custom-icon" data-testid="custom-icon" />}
        >
          Custom icon alert
        </AlertBox>,
      );

      expect(screen.getByTestId("custom-icon")).toBeInTheDocument();
      expect(screen.getByTestId("custom-icon")).toHaveClass("custom-icon");
    });

    it("renders no icon when icon prop is null", () => {
      render(
        <AlertBox variant="error" icon={null}>
          No icon alert
        </AlertBox>,
      );

      const alert = screen.getByRole("alert");
      const svg = alert.querySelector("svg");
      expect(svg).not.toBeInTheDocument();
    });
  });

  describe("Content", () => {
    it("renders string children", () => {
      render(<AlertBox>Simple text message</AlertBox>);

      expect(screen.getByText("Simple text message")).toBeInTheDocument();
    });

    it("renders React node children", () => {
      render(
        <AlertBox>
          <div>
            <strong>Bold text</strong> and <em>italic text</em>
          </div>
        </AlertBox>,
      );

      expect(screen.getByText("Bold text", { exact: false })).toBeInTheDocument();
      expect(screen.getByText("italic text", { exact: false })).toBeInTheDocument();
    });

    it("renders complex nested content", () => {
      render(
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

      expect(screen.getByText("Warning Title")).toBeInTheDocument();
      expect(screen.getByText("This is a warning description with details.")).toBeInTheDocument();
      expect(screen.getByText("Item 1")).toBeInTheDocument();
      expect(screen.getByText("Item 2")).toBeInTheDocument();
    });
  });

  describe("Styling", () => {
    it("applies custom className", () => {
      render(
        <AlertBox className="custom-class mb-8">Message</AlertBox>,
      );

      const alert = screen.getByRole("alert");
      expect(alert).toHaveClass("custom-class", "mb-8");
    });

    it("merges custom className with default classes", () => {
      render(
        <AlertBox variant="error" className="mt-4">
          Message
        </AlertBox>,
      );

      const alert = screen.getByRole("alert");
      expect(alert).toHaveClass("mt-4", "bg-red-50", "border-red-200");
    });

    it("has proper accessibility role", () => {
      render(<AlertBox>Accessible alert</AlertBox>);

      const alert = screen.getByRole("alert");
      expect(alert).toHaveAttribute("role", "alert");
    });

    it("applies base styling classes to all variants", () => {
      const { rerender } = render(<AlertBox variant="error">Error</AlertBox>);

      let alert = screen.getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");

      rerender(<AlertBox variant="success">Success</AlertBox>);
      alert = screen.getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");

      rerender(<AlertBox variant="info">Info</AlertBox>);
      alert = screen.getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");

      rerender(<AlertBox variant="warning">Warning</AlertBox>);
      alert = screen.getByRole("alert");
      expect(alert).toHaveClass("p-3", "border", "rounded-lg");
    });
  });

  describe("Layout", () => {
    it("uses flexbox layout with icon and content", () => {
      render(<AlertBox variant="error">Content</AlertBox>);

      const alert = screen.getByRole("alert");
      const flexContainer = alert.querySelector(".flex");
      expect(flexContainer).toBeInTheDocument();
      expect(flexContainer).toHaveClass("items-start", "gap-2");
    });

    it("renders icon and text in correct order", () => {
      render(
        <AlertBox variant="error">
          <span data-testid="content">Error content</span>
        </AlertBox>,
      );

      const alert = screen.getByRole("alert");
      const flexContainer = alert.querySelector(".flex");
      const children = flexContainer?.children;

      expect(children).toHaveLength(2);
      // First child should be the icon (SVG)
      expect(children?.[0]?.tagName).toBe("svg");
      // Second child should be the content wrapper
      expect(children?.[1]).toContainElement(screen.getByTestId("content"));
    });
  });

  describe("Use Cases", () => {
    it("works as form error display", () => {
      render(
        <AlertBox variant="error">
          Please correct the following errors:
          <ul className="mt-2 list-disc list-inside">
            <li>Email is required</li>
            <li>Password must be at least 8 characters</li>
          </ul>
        </AlertBox>,
      );

      expect(screen.getByText("Please correct the following errors:")).toBeInTheDocument();
      expect(screen.getByText("Email is required")).toBeInTheDocument();
      expect(screen.getByText("Password must be at least 8 characters")).toBeInTheDocument();
    });

    it("works as success notification", () => {
      render(
        <AlertBox variant="success">
          Your changes have been saved successfully!
        </AlertBox>,
      );

      expect(screen.getByText("Your changes have been saved successfully!")).toBeInTheDocument();
    });

    it("works as informational banner", () => {
      render(
        <AlertBox variant="info">
          Maintenance scheduled for tonight at 2 AM EST.
        </AlertBox>,
      );

      expect(screen.getByText("Maintenance scheduled for tonight at 2 AM EST.")).toBeInTheDocument();
    });

    it("works as warning message", () => {
      render(
        <AlertBox variant="warning">
          Your session will expire in 5 minutes. Please save your work.
        </AlertBox>,
      );

      expect(screen.getByText("Your session will expire in 5 minutes. Please save your work.")).toBeInTheDocument();
    });
  });
});
