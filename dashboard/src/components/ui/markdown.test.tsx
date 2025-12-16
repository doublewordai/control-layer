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
      expect(paragraph).toHaveClass("mb-2");
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

      it("should NOT render p tags inside list items", () => {
        const markdown = "- Item 1\n- Item 2\n- Item 3";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const listItems = container.querySelectorAll("li");

        // Check that each list item does NOT contain a p tag
        listItems.forEach((li) => {
          const pTags = li.querySelectorAll("p");
          expect(pTags).toHaveLength(0);
        });
      });

      it("should render list items as direct text children", () => {
        const markdown =
          "- Visual Agent: Description here\n- Visual Coding: More text";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const listItems = container.querySelectorAll("li");

        expect(listItems[0].innerHTML).not.toContain("<p>");
        expect(listItems[1].innerHTML).not.toContain("<p>");
        expect(listItems[0]).toHaveTextContent(
          "Visual Agent: Description here",
        );
        expect(listItems[1]).toHaveTextContent("Visual Coding: More text");
      });

      it("should apply correct classes to ul", () => {
        const markdown = "- Item 1";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const ul = container.querySelector("ul");

        expect(ul).toHaveClass("list-disc");
        expect(ul).toHaveClass("list-inside");
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

      it("should NOT render p tags inside ordered list items", () => {
        const markdown = "1. First item\n2. Second item";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const listItems = container.querySelectorAll("li");

        listItems.forEach((li) => {
          const pTags = li.querySelectorAll("p");
          expect(pTags).toHaveLength(0);
        });
      });

      it("should apply correct classes to ol", () => {
        const markdown = "1. Item 1";
        const { container } = render(<Markdown>{markdown}</Markdown>);
        const ol = container.querySelector("ol");

        expect(ol).toHaveClass("list-decimal");
        expect(ol).toHaveClass("list-inside");
      });
    });
  });

  describe("Code", () => {
    it("should render inline code", () => {
      const markdown = "This is `inline code` text";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const code = container.querySelector("code");

      expect(code).toBeInTheDocument();
      expect(code).toHaveTextContent("inline code");
    });

    it("should render code blocks without language", () => {
      const markdown = "```\ncode block\n```";
      const { container } = render(<Markdown>{markdown}</Markdown>);
      const pre = container.querySelector("pre");
      const code = container.querySelector("code");

      expect(pre).toBeInTheDocument();
      expect(code).toBeInTheDocument();
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

  describe("Complex Content", () => {
    it("should render mixed content without p tags in list items", () => {
      const markdown = `# Title

Paragraph text here.

- List item one
- List item two with **bold** text
- List item three

Another paragraph.`;

      const { container } = render(<Markdown>{markdown}</Markdown>);

      // Check list items don't have p tags
      const listItems = container.querySelectorAll("li");
      listItems.forEach((li) => {
        expect(li.querySelectorAll("p")).toHaveLength(0);
      });

      // Check paragraphs outside lists still render
      const paragraphs = container.querySelectorAll("p");
      expect(paragraphs.length).toBeGreaterThan(0);
    });

    it("should handle nested content in lists correctly", () => {
      const markdown = `Key Enhancements:

- Visual Agent: Operates PC/mobile GUIs—recognizes elements, understands functions, invokes tools, completes tasks.
- Visual Coding Boost: Generates Draw.io/HTML/CSS/JS from images/videos.
- Advanced Spatial Perception: Judges object positions, viewpoints, and occlusions.`;

      const { container } = render(<Markdown>{markdown}</Markdown>);
      const listItems = container.querySelectorAll("li");

      expect(listItems).toHaveLength(3);

      // Ensure no p tags in any list item
      listItems.forEach((li) => {
        const pTags = li.querySelectorAll("p");
        expect(pTags).toHaveLength(0);
        // Check that the text content is directly in the li
        expect(li.innerHTML).not.toContain("<p>");
      });
    });
  });

  describe("Custom Classes", () => {
    it("should apply custom className", () => {
      const { container } = render(
        <Markdown className="custom-class">Text</Markdown>,
      );
      const wrapper = container.firstChild;

      expect(wrapper).toHaveClass("custom-class");
      expect(wrapper).toHaveClass("prose");
      expect(wrapper).toHaveClass("prose-sm");
    });
  });
});
