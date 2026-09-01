import type { Batch } from "./types";

/**
 * Model label for the batches table and detail Metrics card. Prefers the
 * typed `model` field (stamped by dwctl at creation) and falls back to
 * caller-supplied `metadata.model` so older batches still surface one.
 * Returns null when neither is present so the UI can render a dash.
 */
export function batchModelName(batch: Batch): string | null {
  const fromField = batch.model?.trim();
  if (fromField) return fromField;
  const fromMeta = batch.metadata?.model?.trim();
  if (fromMeta) return fromMeta;
  return null;
}
