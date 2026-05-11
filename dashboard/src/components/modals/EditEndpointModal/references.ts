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
  return (
    refs.directHosted.length +
    refs.virtualModels.length +
    refs.trafficRules.length
  );
}

/**
 * "Other" references = ones the user actively configured outside of importing
 * the deployment. The deployment's own implicit Standard Model wrapper is
 * always present and shouldn't trigger a warning, but extra wrappers (or any
 * virtual model / traffic rule) deserve attention.
 */
export function hasUserConfiguredReferences(refs: DeploymentReferences): boolean {
  return (
    refs.directHosted.length > 1 ||
    refs.virtualModels.length > 0 ||
    refs.trafficRules.length > 0
  );
}

// ---------------------------------------------------------------------------
// Index-based lookup (O(1) per query) — used in render-hot paths.
// ---------------------------------------------------------------------------

export interface ReferenceIndex {
  /** key: `${endpointId}|${modelName}` -> Standard Models wrapping that deployment */
  hostedByDeployment: Map<string, Model[]>;
  /** key: wrapper Model.id -> Virtual Models including it as a component */
  virtualByComponentId: Map<string, Model[]>;
  /** key: redirect target alias -> { model, rule } pairs */
  rulesByTarget: Map<string, { model: Model; rule: TrafficRoutingRule }[]>;
}

const EMPTY_INDEX: ReferenceIndex = {
  hostedByDeployment: new Map(),
  virtualByComponentId: new Map(),
  rulesByTarget: new Map(),
};

export function emptyReferenceIndex(): ReferenceIndex {
  return EMPTY_INDEX;
}

/**
 * Build a reference index from a list of all models in the org. The cost is
 * O(M + V + R) where M = models, V = total components, R = total traffic
 * rules. Build once per `allModels` change; do per-deployment lookups against
 * the resulting index.
 */
export function buildReferenceIndex(allModels: Model[]): ReferenceIndex {
  const hostedByDeployment = new Map<string, Model[]>();
  const virtualByComponentId = new Map<string, Model[]>();
  const rulesByTarget = new Map<
    string,
    { model: Model; rule: TrafficRoutingRule }[]
  >();

  for (const m of allModels) {
    // Standard models always have model_name + hosted_on per the type, but
    // we guard defensively in case mock or partially-hydrated data arrives.
    if (!m.is_composite && m.hosted_on && m.model_name) {
      const key = `${m.hosted_on}|${m.model_name}`;
      const list = hostedByDeployment.get(key);
      if (list) list.push(m);
      else hostedByDeployment.set(key, [m]);
    }

    if (m.is_composite && m.components) {
      for (const c of m.components) {
        if (!c.model?.id) continue;
        const list = virtualByComponentId.get(c.model.id);
        if (list) list.push(m);
        else virtualByComponentId.set(c.model.id, [m]);
      }
    }

    if (m.traffic_routing_rules) {
      for (const rule of m.traffic_routing_rules) {
        if (rule.action.type !== "redirect") continue;
        const target = rule.action.target;
        const list = rulesByTarget.get(target);
        const entry = { model: m, rule };
        if (list) list.push(entry);
        else rulesByTarget.set(target, [entry]);
      }
    }
  }

  return { hostedByDeployment, virtualByComponentId, rulesByTarget };
}

/**
 * Look up references for a single deployment using a pre-built index. O(1)
 * worst case (modulo bucket sizes, which are small in practice).
 */
export function lookupReferences(
  index: ReferenceIndex,
  endpointId: string,
  modelName: string,
): DeploymentReferences {
  const directHosted =
    index.hostedByDeployment.get(`${endpointId}|${modelName}`) ?? [];

  // Deduplicate virtuals — a single virtual model can include the same
  // wrapper Model.id only once, but it could include two different wrappers
  // for this deployment if the user has manually created multiple.
  const virtualSet = new Map<string, Model>();
  for (const wrapper of directHosted) {
    const list = index.virtualByComponentId.get(wrapper.id);
    if (!list) continue;
    for (const v of list) virtualSet.set(v.id, v);
  }

  const trafficRules: TrafficRuleReference[] = [];
  const seenAliases = new Set<string>();
  for (const wrapper of directHosted) {
    if (seenAliases.has(wrapper.alias)) continue;
    seenAliases.add(wrapper.alias);
    const list = index.rulesByTarget.get(wrapper.alias);
    if (!list) continue;
    for (const r of list) {
      trafficRules.push({
        modelId: r.model.id,
        modelAlias: r.model.alias,
        rule: r.rule,
      });
    }
  }

  return {
    directHosted: directHosted.map((m) => ({
      modelId: m.id,
      modelAlias: m.alias,
    })),
    virtualModels: Array.from(virtualSet.values()).map((m) => ({
      modelId: m.id,
      modelAlias: m.alias,
    })),
    trafficRules,
  };
}

/**
 * Convenience wrapper: build the index and look up a single deployment in one
 * call. Prefer {@link buildReferenceIndex} + {@link lookupReferences} when
 * you need to look up many deployments against the same model list.
 */
export function computeReferencesForDeployment(
  endpointId: string,
  modelName: string,
  allModels: Model[],
): DeploymentReferences {
  return lookupReferences(buildReferenceIndex(allModels), endpointId, modelName);
}
