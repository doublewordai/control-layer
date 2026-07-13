# Configurable Reasoning Translation Design

## Goal

Expose only OpenAI-compatible reasoning controls to callers while allowing platform operators to configure how each upstream provider receives those controls.

## Public Contract

- Chat Completions accepts `reasoning_effort`.
- Responses accepts `reasoning.effort`.
- Legacy Completions rejects reasoning controls with an OpenAI-shaped `400` error.
- Supported canonical efforts are `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.
- Omitting an effort preserves the upstream provider's default behavior and injects no fields.
- Provider-native controls such as `chat_template_kwargs.thinking` are configuration details, not part of the public API.

## Configuration

An inference endpoint may define a default translation for Chat Completions and Responses. A standard model deployment may inherit that endpoint default or replace it with a model-specific override.

```json
{
  "chat_completions": {
    "target_path": "/chat_template_kwargs/thinking",
    "values": {
      "none": false,
      "low": true,
      "medium": true,
      "high": true
    }
  }
}
```

`target_path` is a constrained JSON pointer. Each configured effort maps to an arbitrary JSON scalar or object, allowing boolean controls and object-shaped provider dialects without arbitrary request patching.

The effective provider translation is the deployment override when present, otherwise the endpoint default. Existing endpoints and deployments default to no translation, preserving passthrough behavior.

## Validation

Administrative writes reject:

- Empty maps or unknown effort names.
- Paths outside reasoning-related roots.
- Paths targeting arrays or protected request fields.
- Excessively deep or large mapped values.

Runtime validation rejects malformed canonical effort values. When a provider translation is configured, it also rejects efforts absent from that map. Composite pools validate against every configured provider before sending a request, so fallback behavior cannot weaken the public contract.

## Request Flow

1. Parse and validate the canonical effort for the API surface.
2. Resolve the model and provider pool.
3. Validate that every provider in the pool can represent the requested effort.
4. For each attempt, clone the canonical body and rewrite the downstream model.
5. Remove the canonical effort when the configured target differs.
6. Deep-set the provider-specific mapped value.
7. Send the translated body.

Responses requests using the Chat adapter carry `reasoning.effort` into Chat's canonical `reasoning_effort`, then use the provider's Chat translation.

## Console

The endpoint and model editors expose a Reasoning Translation section with:

- Separate Chat Completions and Responses mappings.
- A target JSON path.
- One JSON value per canonical effort.
- An inherit/override choice for standard models.
- A canonical-input/downstream-output preview.

## Safety And Compatibility

- Translation is constrained to reasoning-related paths rather than arbitrary JSON Patch.
- Every fallback attempt starts from the original canonical body.
- No prompt, message, tool, model, or streaming field can be targeted.
- Existing configurations continue to pass canonical fields through unchanged.
- Stored configuration is copied into Onwards provider specifications during normal configuration sync.

