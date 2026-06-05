import { createElement } from "react-syntax-highlighter";
import { dwImageUrl, splitDwImgTokens } from "../../utils/dwImage";

/**
 * A `renderer` for {@link CodeBlock} (react-syntax-highlighter) that turns any
 * `dw-img://<sha256>` token in a JSON body into a hyperlink, in place, while
 * preserving syntax highlighting.
 *
 * The link points at the management-API image endpoint
 * (`GET /admin/api/v1/images/<sha256>`), which 302-redirects to a short-lived
 * signed URL — opening it fetches the original image. A `title` tooltip
 * explains that the token is a stored reference and names the endpoint.
 *
 * `rendererProps` / `rendererNode` are global ambient types from
 * `@types/react-syntax-highlighter`.
 */

function tokenTooltip(sha256: string): string {
  return (
    "Stored image reference — the original image is held in your control plane " +
    `(content hash ${sha256.slice(0, 12)}…). Open to retrieve it via ` +
    `GET ${dwImageUrl(sha256)} (management API; dashboard session or platform API key).`
  );
}

/** Recursively rewrite text nodes, replacing `dw-img://` tokens with anchor
 *  nodes and leaving everything else (and its highlight styling) untouched. */
function linkifyNode(node: rendererNode): rendererNode | rendererNode[] {
  if (node.type === "text" && typeof node.value === "string") {
    const segments = splitDwImgTokens(node.value);
    if (segments.length === 1 && segments[0].kind === "text") {
      return node;
    }
    return segments.map((seg) =>
      seg.kind === "text"
        ? { type: "text", value: seg.value }
        : {
            type: "element",
            tagName: "a",
            properties: {
              className: ["dw-img-link"],
              href: dwImageUrl(seg.sha256),
              target: "_blank",
              rel: "noreferrer noopener",
              title: tokenTooltip(seg.sha256),
              style: { textDecoration: "underline", cursor: "pointer" },
            },
            children: [{ type: "text", value: seg.raw }],
          },
    );
  }
  if (node.children) {
    return { ...node, children: node.children.flatMap(linkifyNode) };
  }
  return node;
}

export function dwImgLinkRenderer({ rows, stylesheet, useInlineStyles }: rendererProps) {
  return rows.map((row, i) =>
    createElement({
      node: { ...row, children: (row.children ?? []).flatMap(linkifyNode) },
      stylesheet,
      useInlineStyles,
      key: `dwimg-row-${i}`,
    }),
  );
}
