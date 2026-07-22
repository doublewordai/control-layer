# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.36.0](https://github.com/doublewordai/control-layer/compare/onwards-v0.35.6...onwards-v0.36.0) (2026-07-22)


### Features

* integrate Fusillade and Onwards into the Rust workspace ([#1325](https://github.com/doublewordai/control-layer/issues/1325)) ([6f712ba](https://github.com/doublewordai/control-layer/commit/6f712ba299eb1144d20e294b11ced08d3ef4224e))


### Bug Fixes

* disambiguate scaled-down backend errors from upstream failures ([#819](https://github.com/doublewordai/control-layer/issues/819)) ([d9dc31f](https://github.com/doublewordai/control-layer/commit/d9dc31f10a1d1b44c8957a8021dde6cbb6986571))
* **dwctl:** pick up onwards 0.21.0 opt-in server-side tool calling ([#888](https://github.com/doublewordai/control-layer/issues/888)) ([49494d3](https://github.com/doublewordai/control-layer/commit/49494d338166f562f528f6b59465dab71d46db69))
* hide reasoning tokens if zero ([#970](https://github.com/doublewordai/control-layer/issues/970)) ([f647cd9](https://github.com/doublewordai/control-layer/commit/f647cd9a70fc5f9ce74a8d184adf1291ceb9118d))
* trigger release for fusillade 8.1.0 claim performance fix ([#844](https://github.com/doublewordai/control-layer/issues/844)) ([b04a194](https://github.com/doublewordai/control-layer/commit/b04a19406c0af84f8080367da7829623b279b785))
* update cost names ([#963](https://github.com/doublewordai/control-layer/issues/963)) ([a671cde](https://github.com/doublewordai/control-layer/commit/a671cde8a491cecc3832b9c27e2a5ad9c810a70d))

## [Unreleased]

## [0.35.6](https://github.com/doublewordai/onwards/compare/v0.35.5...v0.35.6) - 2026-07-21

### Fixed

- bump fusillade 24.0.0 ([#276](https://github.com/doublewordai/onwards/pull/276))

## [0.35.5](https://github.com/doublewordai/onwards/compare/v0.35.4...v0.35.5) - 2026-07-21

### Added

- dispatch edge-translated Responses requests to the chat handler ([#274](https://github.com/doublewordai/onwards/pull/274))

## [0.35.4](https://github.com/doublewordai/onwards/compare/v0.35.3...v0.35.4) - 2026-07-20

### Fixed

- update fusillade to 23.0.1 ([#271](https://github.com/doublewordai/onwards/pull/271))

## [0.35.3](https://github.com/doublewordai/onwards/compare/v0.35.2...v0.35.3) - 2026-07-17

### Added

- fusillade 22 ([#262](https://github.com/doublewordai/onwards/pull/262))

### Fixed

- reject malformed Responses reasoning values ([#253](https://github.com/doublewordai/onwards/pull/253))

## [0.35.2](https://github.com/doublewordai/onwards/compare/v0.35.1...v0.35.2) - 2026-07-17

### Fixed

- served-by attribution follow-ups from review ([#256](https://github.com/doublewordai/onwards/pull/256))

### Other

- *(deps)* update rust crate brotli to v8.0.4 ([#202](https://github.com/doublewordai/onwards/pull/202))
- *(deps)* update rust crate uuid to v1.24.0 ([#203](https://github.com/doublewordai/onwards/pull/203))
- *(deps)* update rust crate hyper to v1.10.1 ([#204](https://github.com/doublewordai/onwards/pull/204))
- *(deps)* update rust crate bon to v3.9.3 ([#206](https://github.com/doublewordai/onwards/pull/206))
- *(deps)* update rust crate bytes to v1.12.1 ([#217](https://github.com/doublewordai/onwards/pull/217))
- *(deps)* update actions/checkout action to v7 ([#233](https://github.com/doublewordai/onwards/pull/233))
- *(deps)* update rust crate anyhow to v1.0.103 ([#248](https://github.com/doublewordai/onwards/pull/248))
- *(deps)* update rust docker tag to v1.97.1 ([#249](https://github.com/doublewordai/onwards/pull/249))

## [0.35.1](https://github.com/doublewordai/onwards/compare/v0.35.0...v0.35.1) - 2026-07-17

### Added

- attach ServedBy response extension identifying the serving upstream ([#254](https://github.com/doublewordai/onwards/pull/254))

## [0.35.0](https://github.com/doublewordai/onwards/compare/v0.34.6...v0.35.0) - 2026-07-14

### Added

- add configurable reasoning translation ([#251](https://github.com/doublewordai/onwards/pull/251))

## [0.34.6](https://github.com/doublewordai/onwards/compare/v0.34.5...v0.34.6) - 2026-07-10

### Other

- align multi-step fusillade dependency ([#246](https://github.com/doublewordai/onwards/pull/246))

## [0.34.5](https://github.com/doublewordai/onwards/compare/v0.34.4...v0.34.5) - 2026-07-08

### Fixed

- forward Responses text format to chat completions ([#244](https://github.com/doublewordai/onwards/pull/244))

## [0.34.4](https://github.com/doublewordai/onwards/compare/v0.34.3...v0.34.4) - 2026-07-07

### Added

- fusillade 20 ([#242](https://github.com/doublewordai/onwards/pull/242))

## [0.34.3](https://github.com/doublewordai/onwards/compare/v0.34.2...v0.34.3) - 2026-07-06

### Fixed

- scrub request ids from strict llm requests ([#240](https://github.com/doublewordai/onwards/pull/240))

## [0.34.2](https://github.com/doublewordai/onwards/compare/v0.34.1...v0.34.2) - 2026-06-30

### Fixed

- stop logging prompt/response/provider-error bodies in the realtime path ([#235](https://github.com/doublewordai/onwards/pull/235))

## [0.34.1](https://github.com/doublewordai/onwards/compare/v0.34.0...v0.34.1) - 2026-06-25

### Added

- *(metrics)* per-model onwards_model_inflight gauge for drain-gated scale-down ([#232](https://github.com/doublewordai/onwards/pull/232))

## [0.34.0](https://github.com/doublewordai/onwards/compare/v0.33.4...v0.34.0) - 2026-06-25

### Added

- removed input caching plumbing ([#226](https://github.com/doublewordai/onwards/pull/226))

## [0.33.4](https://github.com/doublewordai/onwards/compare/v0.33.3...v0.33.4) - 2026-06-24

### Added

- strict /messages alias and preserve choices[].stop_reason ([#229](https://github.com/doublewordai/onwards/pull/229))

## [0.33.3](https://github.com/doublewordai/onwards/compare/v0.33.2...v0.33.3) - 2026-06-24

### Added

- *(fallback)* apply the retry budget above the LB strategy ([#227](https://github.com/doublewordai/onwards/pull/227))

### Fixed

- retry loop triggered for stream when no data returned yet ([#207](https://github.com/doublewordai/onwards/pull/207))

## [0.33.2](https://github.com/doublewordai/onwards/compare/v0.33.1...v0.33.2) - 2026-06-22

### Fixed

- default omitted Responses message type and redact upstream auth header ([#220](https://github.com/doublewordai/onwards/pull/220))

## [0.33.1](https://github.com/doublewordai/onwards/compare/v0.33.0...v0.33.1) - 2026-06-19

### Fixed

- *(deps)* bump fusillade to 19.0.0 ([#221](https://github.com/doublewordai/onwards/pull/221))

## [0.33.0](https://github.com/doublewordai/onwards/compare/v0.32.0...v0.33.0) - 2026-06-18

### Added

- add api_key to ClassifyInput for per-customer cache scoping ([#218](https://github.com/doublewordai/onwards/pull/218))

## [0.32.0](https://github.com/doublewordai/onwards/compare/v0.31.1...v0.32.0) - 2026-06-18

### Added

- cached-input-classification seam (dormant CacheClassifier + usage adapter) ([#215](https://github.com/doublewordai/onwards/pull/215))

## [0.31.1](https://github.com/doublewordai/onwards/compare/v0.31.0...v0.31.1) - 2026-06-15

### Fixed

- *(deps)* allow fusillade 18 ([#213](https://github.com/doublewordai/onwards/pull/213))

## [0.31.0](https://github.com/doublewordai/onwards/compare/v0.30.0...v0.31.0) - 2026-06-11

### Added

- configure max body size ([#210](https://github.com/doublewordai/onwards/pull/210))

## [0.30.0](https://github.com/doublewordai/onwards/compare/v0.29.1...v0.30.0) - 2026-06-03

### Added

- per-upstream opt-in for trace context propagation ([#193](https://github.com/doublewordai/onwards/pull/193))

### Other

- *(deps)* update actions/upload-pages-artifact action to v5 ([#176](https://github.com/doublewordai/onwards/pull/176))
- *(deps)* update rust docker tag to v1.96.0 ([#126](https://github.com/doublewordai/onwards/pull/126))
- *(deps)* update actions/deploy-pages action to v5 ([#164](https://github.com/doublewordai/onwards/pull/164))
- *(deps)* update ubuntu docker tag to v26 ([#191](https://github.com/doublewordai/onwards/pull/191))

## [0.29.1](https://github.com/doublewordai/onwards/compare/v0.29.0...v0.29.1) - 2026-05-29

### Fixed

- *(strict)* surface embedded provider errors in 2xx SSE streams; mask account-class codes ([#199](https://github.com/doublewordai/onwards/pull/199))

## [0.29.0](https://github.com/doublewordai/onwards/compare/v0.28.0...v0.29.0) - 2026-05-27

### Added

- *(fallback)* add inter-attempt backoff with jitter and budget cap ([#196](https://github.com/doublewordai/onwards/pull/196))

## [0.28.0](https://github.com/doublewordai/onwards/compare/v0.27.0...v0.28.0) - 2026-05-18

### Added

- multi-step feature flag use of fusillade ([#188](https://github.com/doublewordai/onwards/pull/188))

### Other

- README section for multi-step Open Responses loop ([#190](https://github.com/doublewordai/onwards/pull/190))

## [0.27.0](https://github.com/doublewordai/onwards/compare/v0.26.1...v0.27.0) - 2026-04-29

### Added

- multi-step response orchestration loop and trait ([#185](https://github.com/doublewordai/onwards/pull/185))

## [0.26.1](https://github.com/doublewordai/onwards/compare/v0.26.0...v0.26.1) - 2026-04-24

### Added

- write id into chat completions ([#183](https://github.com/doublewordai/onwards/pull/183))

## [0.26.0](https://github.com/doublewordai/onwards/compare/v0.25.0...v0.26.0) - 2026-04-23

### Added

- configurable response ID override via request header ([#181](https://github.com/doublewordai/onwards/pull/181))

## [0.25.0](https://github.com/doublewordai/onwards/compare/v0.24.2...v0.25.0) - 2026-04-22

### Added

- add background, service_tier, and Queued status to Open Responses schema ([#179](https://github.com/doublewordai/onwards/pull/179))

## [0.24.2](https://github.com/doublewordai/onwards/compare/v0.24.1...v0.24.2) - 2026-04-15

### Fixed

- preserve reasoning_tokens and cached_tokens in usage conversion ([#174](https://github.com/doublewordai/onwards/pull/174))

## [0.24.1](https://github.com/doublewordai/onwards/compare/v0.24.0...v0.24.1) - 2026-04-15

### Added

- backfill missing strict responses fields ([#175](https://github.com/doublewordai/onwards/pull/175))

## [0.24.0](https://github.com/doublewordai/onwards/compare/v0.23.0...v0.24.0) - 2026-04-09

### Added

- preserve and convert reasoning fields through Responses API adapter ([#172](https://github.com/doublewordai/onwards/pull/172))

## [0.23.0](https://github.com/doublewordai/onwards/compare/v0.22.1...v0.23.0) - 2026-04-08

### Fixed

- preserve reasoning fields in strict mode chat completions ([#170](https://github.com/doublewordai/onwards/pull/170))

## [0.22.1](https://github.com/doublewordai/onwards/compare/v0.22.0...v0.22.1) - 2026-03-31

### Fixed

- Retrieve upstream errors inside sse streams (despite 200 headers) and sanitize then forward them, rather than terminate the stream ([#165](https://github.com/doublewordai/onwards/pull/165))

## [0.22.0](https://github.com/doublewordai/onwards/compare/v0.21.2...v0.22.0) - 2026-03-23

### Added

- fix completions streaming and add responses batch streaming ([#161](https://github.com/doublewordai/onwards/pull/161))

## [0.21.2](https://github.com/doublewordai/onwards/compare/v0.21.1...v0.21.2) - 2026-03-19

### Fixed

- handle fusillade-triggered SSE in strict handlers ([#158](https://github.com/doublewordai/onwards/pull/158))

## [0.21.1](https://github.com/doublewordai/onwards/compare/v0.21.0...v0.21.1) - 2026-03-16

### Fixed

- follow routing rule redirects when checking adapter mode ([#156](https://github.com/doublewordai/onwards/pull/156))

## [0.21.0](https://github.com/doublewordai/onwards/compare/v0.20.0...v0.21.0) - 2026-03-16

### Added

- opt-in server-side tool calling via HostedTool ([#155](https://github.com/doublewordai/onwards/pull/155))

### Other

- release v0.20.0 ([#147](https://github.com/doublewordai/onwards/pull/147))

## [0.20.0](https://github.com/doublewordai/onwards/compare/v0.19.2...v0.20.0) - 2026-03-13

### Added

- [**breaking**] RequestContext-based ToolExecutor trait ([#151](https://github.com/doublewordai/onwards/pull/151))

## [0.19.2](https://github.com/doublewordai/onwards/compare/v0.19.1...v0.19.2) - 2026-03-11

### Fixed

- replace Span::enter() with .instrument() in async retry loop ([#145](https://github.com/doublewordai/onwards/pull/145))

## [0.19.1](https://github.com/doublewordai/onwards/compare/v0.19.0...v0.19.1) - 2026-03-11

### Fixed

- use reqwest-blocking-client for OTLP batch export ([#144](https://github.com/doublewordai/onwards/pull/144))

### Other

- release v0.19.0 ([#140](https://github.com/doublewordai/onwards/pull/140))

## [0.19.0](https://github.com/doublewordai/onwards/compare/v0.18.6...v0.19.0) - 2026-03-11

### Added

- otel tracing: clean commit with only relevant files ([#130](https://github.com/doublewordai/onwards/pull/130))

### Fixed

- *(errors)* correct forbidden error response body ([#138](https://github.com/doublewordai/onwards/pull/138))

### Other

- *(deps)* update docker/login-action action to v4 ([#123](https://github.com/doublewordai/onwards/pull/123))
- *(deps)* update docker/build-push-action action to v7 ([#127](https://github.com/doublewordai/onwards/pull/127))

## [0.18.6](https://github.com/doublewordai/onwards/compare/v0.18.5...v0.18.6) - 2026-03-10

### Fixed

- preserve error code in strict mode for internal errors ([#136](https://github.com/doublewordai/onwards/pull/136))

## [0.18.5](https://github.com/doublewordai/onwards/compare/v0.18.4...v0.18.5) - 2026-03-10

### Fixed

- return 503 when model has no providers ([#133](https://github.com/doublewordai/onwards/pull/133))
- use PAT in release-plz so Docker build triggers ([#134](https://github.com/doublewordai/onwards/pull/134))

## [0.18.4](https://github.com/doublewordai/onwards/compare/v0.18.3...v0.18.4) - 2026-03-09

### Fixed

- evaluate routing rules before empty pool check ([#131](https://github.com/doublewordai/onwards/pull/131))

## [0.18.3](https://github.com/doublewordai/onwards/compare/v0.18.2...v0.18.3) - 2026-03-09

### Added

- return 503 for connection errors to disambiguate scaled-down backends ([#128](https://github.com/doublewordai/onwards/pull/128))

## [0.18.2](https://github.com/doublewordai/onwards/compare/v0.18.1...v0.18.2) - 2026-03-04

### Added

- add POST /v1/completions to strict router ([#121](https://github.com/doublewordai/onwards/pull/121))

## [0.18.1](https://github.com/doublewordai/onwards/compare/v0.18.0...v0.18.1) - 2026-02-27

### Fixed

- preserve request counts across config updates ([#118](https://github.com/doublewordai/onwards/pull/118))

## [0.18.0](https://github.com/doublewordai/onwards/compare/v0.17.1...v0.18.0) - 2026-02-25

### Added

- replace weighted random with weighted least connections load balancing ([#114](https://github.com/doublewordai/onwards/pull/114))

## [0.17.1](https://github.com/doublewordai/onwards/compare/v0.17.0...v0.17.1) - 2026-02-24

### Fixed

- remove /v1 prefix from strict mode routes to prevent double /v1 path ([#115](https://github.com/doublewordai/onwards/pull/115))

### Other

- *(ci)* stop running heavy CI on main pushes ([#112](https://github.com/doublewordai/onwards/pull/112))

## [0.17.0](https://github.com/doublewordai/onwards/compare/v0.16.1...v0.17.0) - 2026-02-23

### Added

- add routing rules for per-key traffic control ([#103](https://github.com/doublewordai/onwards/pull/103))

## [0.16.1](https://github.com/doublewordai/onwards/compare/v0.16.0...v0.16.1) - 2026-02-23

### Fixed

- maybe fix concurrency bug with Dashmap ([#109](https://github.com/doublewordai/onwards/pull/109))

## [0.16.0](https://github.com/doublewordai/onwards/compare/v0.15.2...v0.16.0) - 2026-02-23

### Added

- add Open Responses adapter for protocol translation ([#79](https://github.com/doublewordai/onwards/pull/79))

### Fixed

- trusted providers ([#107](https://github.com/doublewordai/onwards/pull/107))

## [0.15.2](https://github.com/doublewordai/onwards/compare/v0.15.1...v0.15.2) - 2026-02-19

### Fixed

- HttpConnector enforcing http test and SSE handling in strict mode ([#102](https://github.com/doublewordai/onwards/pull/102))

## [0.15.1](https://github.com/doublewordai/onwards/compare/v0.15.0...v0.15.1) - 2026-02-18

### Fixed

- Set `enforce_http` to `false` to allow non-http URI schemes in `… ([#100](https://github.com/doublewordai/onwards/pull/100))

### Other

- *(deps)* update rust docker tag to v1.93.1 ([#85](https://github.com/doublewordai/onwards/pull/85))

## [0.15.0](https://github.com/doublewordai/onwards/compare/v0.14.0...v0.15.0) - 2026-02-17

### Added

- *(strict-mode)* add trusted pools to bypass sanitization ([#88](https://github.com/doublewordai/onwards/pull/88))

## [0.14.0](https://github.com/doublewordai/onwards/compare/v0.13.0...v0.14.0) - 2026-02-16

### Added

- improved connection pooling, reuse, holds idles, cleanups, timeouts ([#65](https://github.com/doublewordai/onwards/pull/65))

## [0.13.0](https://github.com/doublewordai/onwards/compare/v0.12.0...v0.13.0) - 2026-02-16

### Added

- add with-replacement sampling for weighted random failover ([#93](https://github.com/doublewordai/onwards/pull/93))

## [0.12.0](https://github.com/doublewordai/onwards/compare/v0.11.2...v0.12.0) - 2026-02-13

### Added

- add strict mode router with schema validation ([#78](https://github.com/doublewordai/onwards/pull/78))
- openAI-style error sanitization ([#84](https://github.com/doublewordai/onwards/pull/84))

### Other

- *(deps)* update rust crate anyhow to v1.0.101 ([#81](https://github.com/doublewordai/onwards/pull/81))
- *(deps)* update rust docker tag to v1.93.0 ([#63](https://github.com/doublewordai/onwards/pull/63))
- *(deps)* update actions/checkout action to v6 ([#76](https://github.com/doublewordai/onwards/pull/76))
- *(deps)* update rust crate hyper-util to v0.1.20 ([#70](https://github.com/doublewordai/onwards/pull/70))
- *(deps)* update rust crate clap to v4.5.57 ([#66](https://github.com/doublewordai/onwards/pull/66))
- *(deps)* update actions/upload-pages-artifact action to v4 ([#77](https://github.com/doublewordai/onwards/pull/77))
- *(deps)* update rust crate bytes to v1.11.1 ([#72](https://github.com/doublewordai/onwards/pull/72))
- replace monolithic README with mdBook documentation site ([#74](https://github.com/doublewordai/onwards/pull/74))

## [0.11.2](https://github.com/doublewordai/onwards/compare/v0.11.1...v0.11.2) - 2026-02-02

### Added

- furnish more useful info on to traces ([#67](https://github.com/doublewordai/onwards/pull/67))

### Other

- enforce conventional commits in pr titles ([#69](https://github.com/doublewordai/onwards/pull/69))

## [0.11.1](https://github.com/doublewordai/onwards/compare/v0.11.0...v0.11.1) - 2026-01-26

### Fixed

- *(deps)* update rust crate axum-prometheus to 0.10.0 ([#54](https://github.com/doublewordai/onwards/pull/54))

### Other

- Buffer SSE chunks to handle incomplete JSON from providers ([#64](https://github.com/doublewordai/onwards/pull/64))
- *(deps)* update rust crate axum-test to v18 ([#57](https://github.com/doublewordai/onwards/pull/57))
- *(deps)* update ubuntu docker tag to v24 ([#58](https://github.com/doublewordai/onwards/pull/58))
- *(deps)* update docker/build-push-action action to v6 ([#56](https://github.com/doublewordai/onwards/pull/56))
- *(deps)* update actions/checkout action to v6 ([#55](https://github.com/doublewordai/onwards/pull/55))
- *(deps)* update rust docker tag to v1.92.0 ([#53](https://github.com/doublewordai/onwards/pull/53))

## [0.11.0](https://github.com/doublewordai/onwards/compare/v0.10.1...v0.11.0) - 2026-01-16

### Added

- [**breaking**] chat completion sanitisation ([#59](https://github.com/doublewordai/onwards/pull/59))

## [0.10.1](https://github.com/doublewordai/onwards/compare/v0.10.0...v0.10.1) - 2026-01-15

### Added

- return 502 Bad Gateway for empty provider pools

## [0.10.0](https://github.com/doublewordai/onwards/compare/v0.9.2...v0.10.0) - 2026-01-13

### Added

- Add load balancing for multiple downstream providers ([#47](https://github.com/doublewordai/onwards/pull/47))

### Other

- *(deps)* update rust crate hyper to v1.8.1 ([#46](https://github.com/doublewordai/onwards/pull/46))
- add fallback and pool-level configuration documentation ([#48](https://github.com/doublewordai/onwards/pull/48))
- *(deps)* update rust crate anyhow to v1.0.100 ([#34](https://github.com/doublewordai/onwards/pull/34))
- *(deps)* update rust crate async-trait to v0.1.89 ([#35](https://github.com/doublewordai/onwards/pull/35))
- Add renovate.json ([#32](https://github.com/doublewordai/onwards/pull/32))

## [0.9.2](https://github.com/doublewordai/onwards/compare/v0.9.1...v0.9.2) - 2025-11-14

### Added

- global and per-key concurrency limits

### Other

- Merge branch 'main' of https://github.com/doublewordai/onwards

## [0.9.1](https://github.com/doublewordai/onwards/compare/v0.9.0...v0.9.1) - 2025-11-06

### Added

- add response headers ([#29](https://github.com/doublewordai/onwards/pull/29))

## [0.9.0](https://github.com/doublewordai/onwards/compare/v0.8.4...v0.9.0) - 2025-10-30

### Added

- configurable auth header names

## [0.8.4](https://github.com/doublewordai/onwards/compare/v0.8.3...v0.8.4) - 2025-10-27

### Added

- strip matching prefixes

## [0.8.3](https://github.com/doublewordai/onwards/compare/v0.8.2...v0.8.3) - 2025-10-27

### Added

- normalize all urls

## [0.8.2](https://github.com/doublewordai/onwards/compare/v0.8.1...v0.8.2) - 2025-10-17

### Added

- deal with incoming headers properly

## [0.8.1](https://github.com/doublewordai/onwards/compare/v0.8.0...v0.8.1) - 2025-09-26

### Other

- fix missing import in doctest

## [0.8.0](https://github.com/doublewordai/onwards/compare/v0.7.1...v0.8.0) - 2025-09-26

### Added

- add body transformation with path parameter ([#21](https://github.com/doublewordai/onwards/pull/21))

### Other

- update documentation

## [0.7.1](https://github.com/doublewordai/onwards/compare/v0.7.0...v0.7.1) - 2025-09-25

### Fixed

- add builder for Auth

## [0.7.0](https://github.com/doublewordai/onwards/compare/v0.6.1...v0.7.0) - 2025-09-25

### Added

- add per key limits option ([#19](https://github.com/doublewordai/onwards/pull/19))

## [0.6.1](https://github.com/doublewordai/onwards/compare/v0.6.0...v0.6.1) - 2025-09-17

### Added

- add /models endpoint alongside /v1/models so that we can consistently proxy to endpoints (like gemini) that dont use the /v1 prefix

### Other

- Merge branch 'main' of https://github.com/doublewordai/onwards

## [0.6.0](https://github.com/doublewordai/onwards/compare/v0.5.0...v0.6.0) - 2025-09-08

### Added

- rate limits + structured errors ([#16](https://github.com/doublewordai/onwards/pull/16))

### Other

- Rename autolabel to autolabel.yaml
- Create autolabel

## [0.5.0](https://github.com/doublewordai/onwards/compare/v0.4.0...v0.5.0) - 2025-08-18

### Fixed

- filter /v1/models endpoint by bearer token permissions ([#15](https://github.com/doublewordai/onwards/pull/15))

### Other

- add release process documentation
