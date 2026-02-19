import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { Markdown } from "./markdown";

describe("Markdown Component", () => {
  describe("Paragraphs", () => {
    it("should render a simple paragraph", () => {
      const { container } = render(<Markdown>This is a paragraph</Markdown>);
      const paragraph = container.querySelector("p");
      expect(paragraph).toBeInTheDocument();
      expect(paragraph).toHaveTextContent("This is a paragraph");
    });

    it("should render multiple paragraphs with spacing", () => {
      const markdown = "First paragraph\n\nSecond paragraph";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const paragraphs = container.querySelectorAll("p");
      expect(paragraphs).toHaveLength(2);
      expect(paragraphs[0]).toHaveTextContent("First paragraph");
      expect(paragraphs[1]).toHaveTextContent("Second paragraph");
    });

    it("should use compact spacing in compact mode", () => {
      const { container } = render(<Markdown compact>Paragraph</Markdown>);
      const paragraph = container.querySelector("p");
      expect(paragraph).toHaveClass("mb-1");
    });

    it("should use normal spacing in normal mode", () => {
      const { container } = render(<Markdown>Paragraph</Markdown>);
      const paragraph = container.querySelector("p");
      expect(paragraph).toHaveClass("mb-3");
    });
  });

  describe("Headings", () => {
    it("should render headings with distinct sizes", () => {
      const markdown = "# H1\n\n## H2\n\n### H3\n\n#### H4";
      const { container } = render(<Markdown>{markdown}</Markdown>);

      const h1 = container.querySelector("h1");
      const h2 = container.querySelector("h2");
      const h3 = container.querySelector("h3");
      const h4 = container.querySelector("h4");

      expect(h1).toHaveClass("text-xl", "font-semibold");
      expect(h2).toHaveClass("text-lg", "font-semibold");
      expect(h3).toHaveClass("text-base", "font-semibold");
      expect(h4).toHaveClass("text-sm", "font-semibold");
    });
  });

  describe("Lists", () => {
    describe("Unordered Lists", () => {
      it("should render a bulleted list", () => {
        const markdown = "- Item 1\n- Item 2\n- Item 3";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const ul = container.querySelector("ul");
        const listItems = container.querySelectorAll("li");

        expect(ul).toBeInTheDocument();
        expect(listItems).toHaveLength(3);
        expect(listItems[0]).toHaveTextContent("Item 1");
        expect(listItems[1]).toHaveTextContent("Item 2");
        expect(listItems[2]).toHaveTextContent("Item 3");
      });

      it("should apply correct classes to ul", () => {
        const markdown = "- Item 1";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const ul = container.querySelector("ul");

        expect(ul).toHaveClass("list-disc");
        expect(ul).toHaveClass("pl-5");
      });

      it("should use compact spacing for ul in compact mode", () => {
        const markdown = "- Item 1";
        const { container } = render(<Markdown compact>{markdown}</Markdown>);
        const ul = container.querySelector("ul");

        expect(ul).toHaveClass("mb-1");
        expect(ul).toHaveClass("space-y-0.5");
      });
    });

    describe("Ordered Lists", () => {
      it("should render a numbered list", () => {
        const markdown = "1. First\n2. Second\n3. Third";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const ol = container.querySelector("ol");
        const listItems = container.querySelectorAll("li");

        expect(ol).toBeInTheDocument();
        expect(listItems).toHaveLength(3);
      });

      it("should apply correct classes to ol", () => {
        const markdown = "1. Item 1";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const ol = container.querySelector("ol");

        expect(ol).toHaveClass("list-decimal");
        expect(ol).toHaveClass("pl-5");
      });
    });
  });

  describe("Code", () => {
    it("should render inline code with inline styling", () => {
      const markdown = "This is `inline code` text";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const code = container.querySelector("code");

      expect(code).toBeInTheDocument();
      expect(code).toHaveTextContent("inline code");
      expect(code).toHaveClass("bg-gray-100", "font-mono");
    });

    it("should render fenced code blocks without language", () => {
      const markdown = "```\ncode block\n```";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const pre = container.querySelector("pre");

      expect(pre).toBeInTheDocument();
      expect(pre).toHaveClass("bg-gray-900");
    });

    it("should not render inline code as a block", () => {
      const markdown = "Use `const x = 1` in your code";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const pre = container.querySelector("pre");
      const code = container.querySelector("code");

      expect(pre).not.toBeInTheDocument();
      expect(code).toHaveClass("bg-gray-100");
    });
  });

  describe("Links", () => {
    it("should render links with correct attributes", () => {
      const markdown = "[Link text](https://example.com)";
      render(<Markdown>{markdown}</Markdown>);
      const link = screen.getByRole("link", { name: "Link text" });

      expect(link).toHaveAttribute("href", "https://example.com");
      expect(link).toHaveAttribute("target", "_blank");
      expect(link).toHaveAttribute("rel", "noopener noreferrer");
      expect(link).toHaveClass("text-blue-600");
    });
  });

  describe("Tables", () => {
    it("should render tables with proper structure", () => {
      const markdown =
        "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
      const { container } = render(<Markdown>{markdown}</Markdown>);

      expect(container.querySelector("table")).toBeInTheDocument();
      expect(container.querySelector("thead")).toHaveClass("bg-gray-50");
      expect(container.querySelectorAll("th")).toHaveLength(2);
      expect(container.querySelectorAll("td")).toHaveLength(4);
    });
  });

  describe("Blockquotes", () => {
    it("should render blockquotes with styling", () => {
      const markdown = "> This is a quote";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const blockquote = container.querySelector("blockquote");

      expect(blockquote).toBeInTheDocument();
      expect(blockquote).toHaveClass("border-l-2", "italic");
    });
  });

  describe("Complex Content", () => {
    it("should render mixed content correctly", () => {
      const markdown = `# Title

Paragraph text here.

- List item one
- List item two with **bold** text
- List item three

Another paragraph.`;

      const { container } = render(<Markdown>{markdown}</Markdown>);

      expect(container.querySelector("h1")).toHaveTextContent("Title");
      expect(container.querySelectorAll("li")).toHaveLength(3);
      expect(container.querySelectorAll("p").length).toBeGreaterThan(0);
    });
  });

  describe("Custom Classes", () => {
    it("should apply custom className", () => {
      const { container } = render(
        <Markdown className="custom-class">Text</Markdown>,
      );
      const wrapper = container.firstChild;

      expect(wrapper).toHaveClass("custom-class");
      expect(wrapper).toHaveClass("max-w-none");
    });
  });
});
