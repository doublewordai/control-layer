/**
 * Helpers for the `dw-img://<sha256>` image-reference scheme.
 *
 * When image-input normalization is enabled, image URLs and inline base64
 * `data:` URIs in stored request bodies are replaced with opaque
 * `dw-img://<sha256>` tokens — the original bytes are kept in the control
 * plane's object storage, addressed by content hash. The bytes are resolved
 * (per-user authorized) through the management API, which 302-redirects to a
 * short-lived signed URL.
 */

/** Resolve a content hash to its management-API image endpoint. The browser
 *  follows the 302 to a short-lived signed URL using the session cookie, so
 *  this works directly as an `<img src>`. */
export function dwImageUrl(sha256: string): string {
  return `/admin/api/v1/images/${sha256}`;
}

/** Collect the unique sha256 hashes of every `dw-img://` token in a request
 *  body. Accepts either a JSON string or an already-parsed value and matches
 *  tokens anywhere in the structure, so it works for both the chat-completions
 *  (`image_url.url`) and responses (`image_url`) shapes without coupling to
 *  either. Returns lowercase hashes, de-duplicated, in first-seen order. */
export function extractDwImageShas(body: unknown): string[] {
  let text: string;
  if (typeof body === "string") {
    text = body;
  } else if (body === null || body === undefined) {
    return [];
  } else {
    try {
      text = JSON.stringify(body);
    } catch {
      return [];
    }
  }

  // A token is the scheme followed by exactly 64 lowercase hex chars.
  const re = /dw-img:\/\/([a-f0-9]{64})/gi;
  const shas = new Set<string>();
  for (const match of text.matchAll(re)) {
    shas.add(match[1].toLowerCase());
  }
  return [...shas];
}
