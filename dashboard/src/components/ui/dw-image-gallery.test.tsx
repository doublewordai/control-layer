import { describe, it, expect } from "vitest";
import { render, within } from "@testing-library/react";
import { DwImageGallery } from "./dw-image-gallery";

const SHA = "a".repeat(64);
const SHA2 = "b".repeat(64);

describe("DwImageGallery", () => {
  it("renders one thumbnail per token, pointing at the management-API endpoint", () => {
    const body = {
      messages: [
        { content: [{ type: "image_url", image_url: { url: `dw-img://${SHA}` } }] },
      ],
    };
    const { container } = render(<DwImageGallery body={body} />);
    const imgs = within(container).getAllByRole("img");
    expect(imgs).toHaveLength(1);
    expect(imgs[0]).toHaveAttribute("src", `/admin/api/v1/images/${SHA}`);
  });

  it("de-duplicates and shows a count for multiple images", () => {
    const body = { a: `dw-img://${SHA}`, b: `dw-img://${SHA2}`, c: `dw-img://${SHA}` };
    const { container } = render(<DwImageGallery body={body} />);
    expect(within(container).getAllByRole("img")).toHaveLength(2);
    expect(within(container).getByText("Images (2)")).toBeInTheDocument();
  });

  it("renders nothing when the body has no image tokens", () => {
    const { container } = render(
      <DwImageGallery body={{ messages: [{ content: "no images" }] }} />,
    );
    expect(container).toBeEmptyDOMElement();
  });
});
