import { describe, it, expect } from "vitest";
import { render, within } from "@testing-library/react";
import { CodeBlock } from "./code-block";
import { dwImgLinkRenderer } from "./dw-img-linkify";

const SHA = "a".repeat(64);

describe("dwImgLinkRenderer", () => {
  it("renders a dw-img token as a link to the management-API endpoint", () => {
    const json = JSON.stringify({ image_url: { url: `dw-img://${SHA}` } }, null, 2);
    const { container } = render(
      <CodeBlock language="json" renderer={dwImgLinkRenderer}>
        {json}
      </CodeBlock>,
    );
    const link = within(container).getByRole("link");
    expect(link).toHaveAttribute("href", `/admin/api/v1/images/${SHA}`);
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveTextContent(`dw-img://${SHA}`);
    // The tooltip references the retrieval endpoint.
    expect(link.getAttribute("title")).toContain(`/admin/api/v1/images/${SHA}`);
  });

  it("links a token with a storage nonce to its full reference", () => {
    const ref = `${SHA}.${"c".repeat(32)}`;
    const json = JSON.stringify({ image_url: { url: `dw-img://${ref}` } }, null, 2);
    const { container } = render(
      <CodeBlock language="json" renderer={dwImgLinkRenderer}>
        {json}
      </CodeBlock>,
    );
    const link = within(container).getByRole("link");
    expect(link).toHaveAttribute("href", `/admin/api/v1/images/${ref}`);
    expect(link).toHaveTextContent(`dw-img://${ref}`);
  });

  it("produces no links when the body has no tokens", () => {
    const json = JSON.stringify({ messages: [{ content: "hello" }] }, null, 2);
    const { container } = render(
      <CodeBlock language="json" renderer={dwImgLinkRenderer}>
        {json}
      </CodeBlock>,
    );
    expect(within(container).queryByRole("link")).toBeNull();
  });
});
