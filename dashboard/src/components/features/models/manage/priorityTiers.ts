import type { ModelComponent } from "../../../../api/control-layer";

export function parseRoutingInteger(
  value: string,
  minimum: number,
  maximum: number,
): number | null {
  if (value.trim() === "") return null;
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed >= minimum && parsed <= maximum
    ? parsed
    : null;
}

export interface PriorityTier {
  priority: number;
  providers: ModelComponent[];
}

export function groupPriorityTiers(
  components: ModelComponent[],
): PriorityTier[] {
  const tiers = new Map<number, ModelComponent[]>();
  for (const component of components) {
    const providers = tiers.get(component.sort_order) ?? [];
    providers.push(component);
    tiers.set(component.sort_order, providers);
  }

  return [...tiers.entries()]
    .sort(([left], [right]) => left - right)
    .map(([priority, providers]) => ({ priority, providers }));
}

export function moveProviderToTier(
  components: ModelComponent[],
  providerId: string,
  priority: number,
): ModelComponent[] {
  return components.map((component) =>
    component.model.id === providerId
      ? { ...component, sort_order: priority }
      : component,
  );
}
