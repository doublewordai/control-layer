/**
 * Helpers for the `dw-img://<sha256>` image-reference scheme.
 *
 * When image-input normalization is enabled, image URLs and inline base64
 * `data:` URIs in stored request bodies are replaced with opaque
 * `dw-img://<sha256>` tokens — the original bytes are kept in the control
 * plane's object storage, addressed by content hash. The token is an internal
 * reference: at dispatch it is re-rendered into a fresh short-lived signed URL
 * for the upstream provider, and in the console it is resolved (per-user
 * authorized) through the management API.
 */

/** A run of plain text, or a `dw-img://` token, produced by
 *  {@link splitDwImgTokens}. */
export type DwImgSegment =
  | { kind: "text"; value: string }
  | { kind: "token"; raw: string; sha256: string };

/** Resolve a content hash to its management-API image endpoint. The endpoint
 *  302-redirects to a short-lived signed URL using the caller's credentials
 *  (dashboard session or a platform-purpose API key). */
export function dwImageUrl(sha256: string): string {
  return `/admin/api/v1/images/${sha256}`;
}

/** Split a string into plain-text runs and `dw-img://<sha256>` tokens, in
 *  order. A token is the scheme followed by exactly 64 lowercase hex chars.
 *  Returns a single text segment when there are no tokens. */
export function splitDwImgTokens(text: string): DwImgSegment[] {
  const re = /dw-img:\/\/([a-f0-9]{64})/gi;
  const segments: DwImgSegment[] = [];
  let cursor = 0;
  for (const match of text.matchAll(re)) {
    const start = match.index ?? 0;
    if (start > cursor) {
      segments.push({ kind: "text", value: text.slice(cursor, start) });
    }
    segments.push({ kind: "token", raw: match[0], sha256: match[1].toLowerCase() });
    cursor = start + match[0].length;
  }
  if (cursor < text.length) {
    segments.push({ kind: "text", value: text.slice(cursor) });
  }
  return segments;
}
