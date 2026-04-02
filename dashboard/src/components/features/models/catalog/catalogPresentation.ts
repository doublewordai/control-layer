import type { Model } from "../../../../api/control-layer/types";

export function getModelOrder(model: Model): number | undefined {
  return model.metadata?.extra?.model_order;
}

export function getCatalogIconInitials(label: string): string {
  const words = label
    .split(/[\s/-]+/)
    .map((word) => word.trim())
    .filter(Boolean);

  if (words.length >= 2) {
    return `${words[0][0]}${words[1][0]}`.toUpperCase();
  }

  const collapsed = words[0] ?? label;
  return collapsed.slice(0, 2).toUpperCase();
}
