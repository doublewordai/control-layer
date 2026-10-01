/**
 * Helpers for the `dw-img://<sha256>.<nonce>` image-reference scheme.
 *
 * When image-input normalization is enabled, image URLs and inline base64
 * `data:` URIs in stored request bodies are replaced with opaque
 * `dw-img://<sha256>.<nonce>` tokens — the original bytes are kept in the
 * control plane's object storage. `sha256` is the content hash; `nonce`
 * identifies the stored copy. Older tokens have no `.<nonce>` suffix. The token is an internal
 * reference: at dispatch it is re-rendered into a fresh short-lived signed URL
 * for the upstream provider, and in the console it is resolved (per-user
 * authorized) through the management API.
 */

/** A run of plain text, or a `dw-img://` token, produced by
 *  {@link splitDwImgTokens}. */
export type DwImgSegment =
  | { kind: "text"; value: string }
  | { kind: "token"; raw: string; sha256: string; ref: string };

/** Resolve a token reference (`<sha256>` or `<sha256>.<nonce>`) to its
 *  management-API image endpoint. The endpoint 302-redirects to a short-lived signed URL using the caller's credentials
 *  (dashboard session or a platform-purpose API key). */
export function dwImageUrl(ref: string): string {
  return `/admin/api/v1/images/${ref}`;
}

/** Split a string into plain-text runs and `dw-img://<sha256>` tokens, in
 *  order. A token is the scheme followed by exactly 64 hex chars, optionally
 *  followed by `.` and a 32-hex-char nonce. A hash followed by anything else
 *  that continues the token (a malformed nonce, extra hex) is left as text
 *  rather than linked to the wrong object.
 *  Returns a single text segment when there are no tokens. */
export function splitDwImgTokens(text: string): DwImgSegment[] {
  const re = /dw-img:\/\/([a-f0-9]{64})(\.[a-f0-9]{32})?(?![.a-f0-9])/gi;
  const segments: DwImgSegment[] = [];
  let cursor = 0;
  for (const match of text.matchAll(re)) {
    const start = match.index ?? 0;
    if (start > cursor) {
      segments.push({ kind: "text", value: text.slice(cursor, start) });
    }
    const sha256 = match[1].toLowerCase();
    const ref = sha256 + (match[2] ?? "").toLowerCase();
    segments.push({ kind: "token", raw: match[0], sha256, ref });
    cursor = start + match[0].length;
  }
  if (cursor < text.length) {
    segments.push({ kind: "text", value: text.slice(cursor) });
  }
  return segments;
}
