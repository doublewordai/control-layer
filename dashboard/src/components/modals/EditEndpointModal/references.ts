import type { Model, TrafficRoutingRule } from "../../../api/control-layer/types";

export interface ReferenceEntry {
  modelId: string;
  modelAlias: string;
}

export interface TrafficRuleReference {
  modelId: string;
  modelAlias: string;
  rule: TrafficRoutingRule;
}

export interface DeploymentReferences {
  /** Standard models that wrap this deployment directly (hosted_on=endpoint, model_name=this) */
  directHosted: ReferenceEntry[];
  /** Virtual models that include any of the directHosted models as a component */
  virtualModels: ReferenceEntry[];
  /** Traffic rules (on any model) whose redirect target is one of the directHosted aliases */
  trafficRules: TrafficRuleReference[];
}

export function emptyReferences(): DeploymentReferences {
  return { directHosted: [], virtualModels: [], trafficRules: [] };
}

export function totalReferenceCount(refs: DeploymentReferences): number {
  return refs.directHosted.length + refs.virtualModels.length + refs.trafficRules.length;
}

/**
 * Given an endpoint id and a provider model name, find every model and traffic
 * rule in the system that references this deployment. Pure function — feed it
 * the full Model list (with components included) and it does the rest.
 */
export function computeReferencesForDeployment(
  endpointId: string,
  modelName: string,
  allModels: Model[],
): DeploymentReferences {
  const directHosted = allModels.filter(
    (m) =>
      !m.is_composite &&
      m.hosted_on === endpointId &&
      m.model_name === modelName,
  );

  const directHostedIds = new Set(directHosted.map((m) => m.id));
  const directHostedAliases = new Set(directHosted.map((m) => m.alias));

  const virtualModels = allModels.filter(
    (m) =>
      m.is_composite &&
      m.components?.some((c) => directHostedIds.has(c.model.id)),
  );

  const trafficRules: TrafficRuleReference[] = [];
  for (const m of allModels) {
    if (!m.traffic_routing_rules) continue;
    for (const rule of m.traffic_routing_rules) {
      if (
        rule.action.type === "redirect" &&
        directHostedAliases.has(rule.action.target)
      ) {
        trafficRules.push({ modelId: m.id, modelAlias: m.alias, rule });
      }
    }
  }

  return {
    directHosted: directHosted.map((m) => ({ modelId: m.id, modelAlias: m.alias })),
    virtualModels: virtualModels.map((m) => ({ modelId: m.id, modelAlias: m.alias })),
    trafficRules,
  };
}
