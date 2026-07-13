# Configurable Reasoning Translation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add strict canonical reasoning controls with configurable provider-specific request translation managed from Control Layer.

**Architecture:** Onwards owns canonical request validation and applies a `ReasoningTranslationConfig` held by each provider immediately before an upstream attempt. Control Layer persists endpoint defaults and deployment overrides, validates administrative writes, resolves the effective config during target sync, and provides a structured console editor.

**Tech Stack:** Rust 2024, Axum, Serde, SQLx/PostgreSQL, React 19, TypeScript, Vitest.

## Global Constraints

- Public callers use only OpenAI-compatible Chat Completions and Responses fields.
- Omitted reasoning controls do not alter downstream request bodies.
- Existing providers without configuration retain passthrough behavior.
- Provider translation runs independently for every retry or fallback attempt.
- Administrative translation paths cannot target non-reasoning request fields.

---

### Task 1: Onwards Reasoning Translation Types

**Files:**
- Create: `onwards/src/reasoning.rs`
- Modify: `onwards/src/lib.rs`
- Modify: `onwards/src/target.rs`

**Interfaces:**
- Produces: `ReasoningEffort`, `ReasoningTranslation`, and `ReasoningTranslationConfig`.
- Produces: validation and body translation methods consumed by the proxy handler.

- [ ] **Step 1: Write failing unit tests** for effort parsing, safe target paths, omitted effort no-op, SGLang boolean translation, object translation, unsupported effort, and legacy Completions rejection.
- [ ] **Step 2: Run `cargo test reasoning::tests -- --nocapture`** and confirm failures are caused by the missing module and behavior.
- [ ] **Step 3: Implement the minimal typed configuration and JSON deep-set logic** with constrained reasoning paths and OpenAI-shaped parameter errors.
- [ ] **Step 4: Run `cargo test reasoning::tests -- --nocapture`** and confirm all focused tests pass.

### Task 2: Per-Attempt Onwards Integration

**Files:**
- Modify: `onwards/src/handlers.rs`
- Modify: `onwards/src/target.rs`
- Modify: `onwards/src/strict/schemas/chat_completions.rs`
- Modify: `onwards/src/strict/adapter.rs`
- Modify: `onwards/src/strict/handlers.rs`

**Interfaces:**
- Consumes: `ReasoningTranslationConfig` from Task 1.
- Produces: canonical validation before upstream I/O and provider-specific bodies within the fallback loop.

- [ ] **Step 1: Write failing request-level tests** proving SGLang translation, different fallback-provider shapes, invalid effort `400`, unsupported mapped effort `400`, no-op omission, and Responses adapter propagation.
- [ ] **Step 2: Run the focused tests** and verify each fails because translation is not wired into routing.
- [ ] **Step 3: Add the canonical Chat field and adapter propagation**, validate the pool, and translate the cloned body after model rewriting for each attempt.
- [ ] **Step 4: Run focused tests and `cargo test`**, then run `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`.

### Task 3: Control Layer Persistence And Validation

**Files:**
- Create: `control-layer/dwctl/migrations/111_add_reasoning_translation.sql`
- Create: `control-layer/dwctl/src/reasoning.rs`
- Modify: `control-layer/dwctl/src/lib.rs`
- Modify: `control-layer/dwctl/src/api/models/inference_endpoints.rs`
- Modify: `control-layer/dwctl/src/db/models/inference_endpoints.rs`
- Modify: `control-layer/dwctl/src/db/handlers/inference_endpoints.rs`
- Modify: `control-layer/dwctl/src/api/handlers/inference_endpoints.rs`
- Modify: `control-layer/dwctl/src/api/models/deployments/mod.rs`
- Modify: `control-layer/dwctl/src/db/models/deployments.rs`
- Modify: `control-layer/dwctl/src/db/handlers/deployments.rs`
- Modify: `control-layer/dwctl/src/api/handlers/deployments.rs`

**Interfaces:**
- Produces: nullable endpoint defaults and nullable deployment overrides using the same serialized contract as Onwards.
- Produces: `validate_reasoning_translation_config` for all administrative writes.

- [ ] **Step 1: Add failing validation and repository tests** for valid SGLang config, protected paths, unknown efforts, endpoint persistence, deployment override persistence, and clearing an override.
- [ ] **Step 2: Apply the migration locally and run focused tests** to confirm the new tests fail for missing fields.
- [ ] **Step 3: Implement shared API types, validation, migration columns, and three-state update semantics** while preserving null defaults.
- [ ] **Step 4: Run focused Rust tests and `cargo fmt --check`**.

### Task 4: Control Layer To Onwards Sync

**Files:**
- Modify: `control-layer/Cargo.toml`
- Modify: `control-layer/Cargo.lock`
- Modify: `control-layer/dwctl/src/sync/onwards_config/mod.rs`

**Interfaces:**
- Consumes: persisted endpoint and deployment configurations from Task 3.
- Produces: effective `ProviderSpec.reasoning` for standard and composite providers.

- [ ] **Step 1: Add failing sync tests** proving deployment override precedence, endpoint inheritance, and distinct component mappings in a composite pool.
- [ ] **Step 2: Run focused sync tests** and confirm the provider config is absent.
- [ ] **Step 3: Select `COALESCE(deployment.reasoning_config, endpoint.reasoning_config)`**, deserialize defensively, convert to Onwards types, and attach it to each provider specification.
- [ ] **Step 4: Run focused sync tests and the Rust test suite**.

### Task 5: Console Reasoning Editor

**Files:**
- Create: `control-layer/dashboard/src/components/features/reasoning/ReasoningTranslationEditor.tsx`
- Create: `control-layer/dashboard/src/components/features/reasoning/ReasoningTranslationEditor.test.tsx`
- Create: `control-layer/dashboard/src/components/features/reasoning/index.ts`
- Modify: `control-layer/dashboard/src/api/control-layer/types.ts`
- Modify: `control-layer/dashboard/src/components/modals/CreateEndpointModal/CreateEndpointModal.tsx`
- Modify: `control-layer/dashboard/src/components/modals/EditEndpointModal/EditEndpointModal.tsx`
- Modify: `control-layer/dashboard/src/components/features/models/manage/ModelInfo.tsx`

**Interfaces:**
- Consumes: `ReasoningTranslationConfig` API fields from Task 3.
- Produces: structured endpoint defaults and model override updates.

- [ ] **Step 1: Write failing component tests** for adding an SGLang mapping, parsing JSON values, previewing nested output, reporting invalid JSON, and selecting model inheritance.
- [ ] **Step 2: Run the focused Vitest file** and verify it fails because the editor is missing.
- [ ] **Step 3: Implement the structured editor and preview**, then connect it to endpoint create/edit and standard model update payloads.
- [ ] **Step 4: Run focused tests, `pnpm typecheck`, `pnpm lint`, and `pnpm test -- --run`**.

### Task 6: End-To-End Verification And Delivery

**Files:**
- Modify: OpenAPI snapshots or generated artifacts only if repository checks require them.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: reviewable Onwards and Control Layer pull requests.

- [ ] **Step 1: Test an SGLang mapping end to end** with canonical `reasoning_effort: "none"` producing `chat_template_kwargs.thinking: false`.
- [ ] **Step 2: Verify omitted effort, invalid effort, unsupported effort, Responses adapter, and mixed-provider fallback behavior**.
- [ ] **Step 3: Run complete repository lint and test commands**, inspect both diffs for unrelated or sensitive context, and confirm clean worktrees.
- [ ] **Step 4: Commit with conventional commits, push both branches, and create dependency-ordered pull requests** with generic public descriptions.

