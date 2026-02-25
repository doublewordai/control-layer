import type { Model } from "../api/control-layer/types";

/**
 * Returns true if the model has a traffic routing rule that denies
 * the "playground" or "realtime" API key purpose. Either denial
 * means the playground will not work for this model.
 */
export function isPlaygroundDenied(model: Model): boolean {
  return (
    model.traffic_routing_rules?.some(
      (rule) =>
        rule.action.type === "deny" &&
        (rule.api_key_purpose === "playground" ||
          rule.api_key_purpose === "realtime"),
    ) ?? false
  );
}

/**
 * Returns true if the model has a traffic routing rule that denies
 * the "batch" API key purpose.
 */
export function isBatchDenied(model: Model): boolean {
  return (
    model.traffic_routing_rules?.some(
      (rule) =>
        rule.action.type === "deny" && rule.api_key_purpose === "batch",
    ) ?? false
  );
}
