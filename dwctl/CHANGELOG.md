# Changelog

## [0.8.0](https://github.com/doublewordai/control-layer/compare/v0.7.1...v0.8.0) (2025-11-24)


### Features

* add actual cancellation of in-progress batch requests ([#170](https://github.com/doublewordai/control-layer/issues/170)) ([2044218](https://github.com/doublewordai/control-layer/commit/2044218ecaffbb763b1cf8750e4d072469b4be62))
* **dwctl:** add pagination to models endpoint and improve files pagination docs ([#177](https://github.com/doublewordai/control-layer/issues/177)) ([440bea6](https://github.com/doublewordai/control-layer/commit/440bea6d91b53ffaefdb481871e271247eb87de3))
* improve the usability of the proxy-header configuration ([#174](https://github.com/doublewordai/control-layer/issues/174)) ([a560508](https://github.com/doublewordai/control-layer/commit/a56050895c1f1475e352357f54e49c61c382e55d))


### Bug Fixes

* broken anthropic ([b3b190f](https://github.com/doublewordai/control-layer/commit/b3b190fb43d974c825a571048b417ed1da65b807))
* issue with duplicate batch daemons on leader ([#183](https://github.com/doublewordai/control-layer/issues/183)) ([fe816a1](https://github.com/doublewordai/control-layer/commit/fe816a16a3777c33ba60947f1be95138361aadfd))

## [0.7.1](https://github.com/doublewordai/control-layer/compare/v0.7.0...v0.7.1) (2025-11-21)


### Bug Fixes

* **dwctl:** improve JWT error code mapping ([#173](https://github.com/doublewordai/control-layer/issues/173)) ([4b22293](https://github.com/doublewordai/control-layer/commit/4b222939f083f1e720d1ea0926a5c53977178470)), closes [#80](https://github.com/doublewordai/control-layer/issues/80)
* SSE parser now correctly handles empty data fields per spec ([#171](https://github.com/doublewordai/control-layer/issues/171)) ([4b9d04c](https://github.com/doublewordai/control-layer/commit/4b9d04c1c7ff0b24d21f0d1af9d29914fd456cf3))
* transaction credit precision + no admin negative access ([#176](https://github.com/doublewordai/control-layer/issues/176)) ([218596c](https://github.com/doublewordai/control-layer/commit/218596cc10637852bf783e259117787b2ea0c2f7))

## [0.7.0](https://github.com/doublewordai/control-layer/compare/v0.6.0...v0.7.0) (2025-11-19)


### Features

* add capacity & batch capacity ([#106](https://github.com/doublewordai/control-layer/issues/106)) ([d7f3f6d](https://github.com/doublewordai/control-layer/commit/d7f3f6d5435717dd10e2fda304bf4022a3179dc8))
* Add support for billing to frontend ([#53](https://github.com/doublewordai/control-layer/issues/53)) ([c4f81da](https://github.com/doublewordai/control-layer/commit/c4f81dac1aec18a2288a0f6678e18c7f8c830d22))
* allow pricing to be updated from frontend ([#111](https://github.com/doublewordai/control-layer/issues/111)) ([a7ab173](https://github.com/doublewordai/control-layer/commit/a7ab1733bd9c7d58c6a93a89de588da519ecbcff))
* batches endpoints ([#72](https://github.com/doublewordai/control-layer/issues/72)) ([f2143c6](https://github.com/doublewordai/control-layer/commit/f2143c6be2ed1cdc1cba60e630259feb1166ab7e))
* caching configuration for static files ([#149](https://github.com/doublewordai/control-layer/issues/149)) ([77818c0](https://github.com/doublewordai/control-layer/commit/77818c064ec43bc75aba9a89420c673e9d6060bd))
* daemon status tracking ([#96](https://github.com/doublewordai/control-layer/issues/96)) ([9222649](https://github.com/doublewordai/control-layer/commit/9222649f6706756fc5166c4747893e356f196914))
* deduct credits when users use api ([#66](https://github.com/doublewordai/control-layer/issues/66)) ([e40ff24](https://github.com/doublewordai/control-layer/commit/e40ff24add8dc3c570e151aed9652126ac833b9e))
* **dwctl:** Validate model access in batch file uploads ([#151](https://github.com/doublewordai/control-layer/issues/151)) ([089aa1a](https://github.com/doublewordai/control-layer/commit/089aa1a3a2583b090275fbe8a7270362dd55a3d5))
* openAI compatible files endpoints ([#60](https://github.com/doublewordai/control-layer/issues/60)) ([5c2eccd](https://github.com/doublewordai/control-layer/commit/5c2eccd3aafc8b2fabe6baadad4d26552a80da41))
* track batch status via triggers, and query in bulk rather than doing N+1 queries ([#100](https://github.com/doublewordai/control-layer/issues/100)) ([68d005d](https://github.com/doublewordai/control-layer/commit/68d005dadb00c2a4afc066b8a62c2afb528d57ef))


### Bug Fixes

* add just release target, setup idempotent publishing ([3084ce1](https://github.com/doublewordai/control-layer/commit/3084ce18c95ddabc23a9716e9918dcb244e51141))
* bug where providing an invalid API key (like we do in the playground) would foreclose other auth methods ([#91](https://github.com/doublewordai/control-layer/issues/91)) ([1627a76](https://github.com/doublewordai/control-layer/commit/1627a7670b1bbd94090fe514c2836c585baf3ee5))
* don't charge system users ([#150](https://github.com/doublewordai/control-layer/issues/150)) ([aaa0196](https://github.com/doublewordai/control-layer/commit/aaa019628f97455d880bfcf98ee3d9914a7759be))
* matching order ([de42c08](https://github.com/doublewordai/control-layer/commit/de42c08aa6578e5b73f582f3229786140b7815dd))
* revert to aggregating batch status on demand ([#112](https://github.com/doublewordai/control-layer/issues/112)) ([04e9498](https://github.com/doublewordai/control-layer/commit/04e9498fc92e2461482f8df016c6b0e4974f0a78))
* Various billing fixes ([#147](https://github.com/doublewordai/control-layer/issues/147)) ([a30a29a](https://github.com/doublewordai/control-layer/commit/a30a29aaec8fed9799da57606b152a7818c81da2))


### Dependencies

* The following workspace dependencies were updated
  * dependencies
    * fusillade bumped from 0.1 to 0.2.0

## [0.6.0](https://github.com/doublewordai/control-layer/compare/v0.5.1...v0.6.0) (2025-11-06)


### Features

* add fusillade: a daemon implementation for sending batched requests ([#55](https://github.com/doublewordai/control-layer/issues/55)) ([af4a60e](https://github.com/doublewordai/control-layer/commit/af4a60ed91c7e7732e6fa16427522e013b86c50b))
* backend Credit API ([#46](https://github.com/doublewordai/control-layer/issues/46)) ([9ea9453](https://github.com/doublewordai/control-layer/commit/9ea9453f17df18d613481e64193c5d61a08280e3))
* OTEL tracing export ([#57](https://github.com/doublewordai/control-layer/issues/57)) ([ced2e12](https://github.com/doublewordai/control-layer/commit/ced2e124e12eb3a1a25d92dba45f398dbea024b6))
* support Cortex AI and SPCS, and also add the ability to manually configure model endpoints. Also, overhaul design of endpoint creation flow ([#51](https://github.com/doublewordai/control-layer/issues/51)) ([5419e31](https://github.com/doublewordai/control-layer/commit/5419e310fd65542d58be76a09ffc130ea8a3f57c))


### Bug Fixes

* an issue where transient db disconnects could kill the onwards listener task ([#59](https://github.com/doublewordai/control-layer/issues/59)) ([5950883](https://github.com/doublewordai/control-layer/commit/5950883b1203abf4b017db9fdd1cdce1039c23a9))
* make tracing configurable ([#58](https://github.com/doublewordai/control-layer/issues/58)) ([b4bea00](https://github.com/doublewordai/control-layer/commit/b4bea004e40270d9a90435496ddd33da22019356))

## [0.5.1](https://github.com/doublewordai/control-layer/compare/v0.5.0...v0.5.1) (2025-10-30)


### Bug Fixes

* annoying log line ([65c39c3](https://github.com/doublewordai/control-layer/commit/65c39c31afdedf3d3c2ef448d4de34bc036364f7))

## [0.5.0](https://github.com/doublewordai/control-layer/compare/v0.4.2...v0.5.0) (2025-10-29)


### Features

* Uptime monitoring via Probes API ([#40](https://github.com/doublewordai/control-layer/issues/40)) ([ae56133](https://github.com/doublewordai/control-layer/commit/ae56133e982c101244152f6cd67eb740a1c9bb11))


### Bug Fixes

* Alias uniqueness enforced across control layer ([#39](https://github.com/doublewordai/control-layer/issues/39)) ([7f3ad57](https://github.com/doublewordai/control-layer/commit/7f3ad57e799498ecc09055aa220813011bde7a49))

## [0.4.2](https://github.com/doublewordai/control-layer/compare/v0.4.1...v0.4.2) (2025-10-21)


### Bug Fixes

* default to embedded db if enabled ([#36](https://github.com/doublewordai/control-layer/issues/36)) ([41c2941](https://github.com/doublewordai/control-layer/commit/41c29415825ae75f81adf5293246b6c117503b04))

## [0.4.1](https://github.com/doublewordai/control-layer/compare/v0.4.0...v0.4.1) (2025-10-21)


### Features

* rename to dwctl ([#34](https://github.com/doublewordai/control-layer/issues/34)) ([043313e](https://github.com/doublewordai/control-layer/commit/043313ef373154399cf3d70d9afaa4596a5d739c))

## [0.4.0](https://github.com/doublewordai/control-layer/compare/v0.3.0...v0.4.0) (2025-10-19)


### Features

* Add the ability for headers to be used to set user groups. Useful for group mapping from downstream proxies ([#27](https://github.com/doublewordai/control-layer/issues/27)) ([16362e9](https://github.com/doublewordai/control-layer/commit/16362e9a61228f80e18afad620e2cc0cc9589963))
* support changing password on the profile tab, and support uploading images in the playground ([#33](https://github.com/doublewordai/control-layer/issues/33)) ([dde9250](https://github.com/doublewordai/control-layer/commit/dde9250704142633c4aa039d9514616b9f4f0c11))

## [0.3.0](https://github.com/doublewordai/control-layer/compare/v0.2.0...v0.3.0) (2025-10-17)


### Features

* anthropic support ([#28](https://github.com/doublewordai/control-layer/issues/28)) ([e6d444b](https://github.com/doublewordai/control-layer/commit/e6d444bdd8b84ca248ba2f17d4b4a30a6522adfc))
* expect '/v1' to be added to the openai api base path. We used to add '/v1/models/' to base paths when we were querying for models from upstream providers, but hereinafter, we'll only add '/models'. That way, we can support APIs that don't expose their openAI compatible APIs under a /v1/ subpath.([#25](https://github.com/doublewordai/control-layer/issues/25)) ([3c5f3e6](https://github.com/doublewordai/control-layer/commit/3c5f3e673f1bd214651673ec98377dd1f8cb3120))


### Bug Fixes

* improve splash page, and add dropdown options for anthropic, gemini, openai ([#29](https://github.com/doublewordai/control-layer/issues/29)) ([7878d6b](https://github.com/doublewordai/control-layer/commit/7878d6ba39d4066bd01e8d2ffdc2c84ae00f1f56))
* make trailing slash behaviour better ([#24](https://github.com/doublewordai/control-layer/issues/24)) ([cfc5335](https://github.com/doublewordai/control-layer/commit/cfc533543dc0ba858d5e6c744a53874fd5558b44))

## [0.2.0](https://github.com/doublewordai/control-layer/compare/v0.1.3...v0.2.0) (2025-10-17)


### Features

* trigger release please ([95a195b](https://github.com/doublewordai/control-layer/commit/95a195bf677a6c09114a23a08e60a28143e112f6))


### Bug Fixes

* better OSS ux, bundle DB, frontend into single binary,  rename to waycast, simplify CI([#6](https://github.com/doublewordai/control-layer/issues/6)) ([dd4bfa3](https://github.com/doublewordai/control-layer/commit/dd4bfa3b3d012be33055402805a317b3a7e7766a))
* docs change to trigger release please ([#18](https://github.com/doublewordai/control-layer/issues/18)) ([8d2ae51](https://github.com/doublewordai/control-layer/commit/8d2ae51be6b26b01300c9a3484c484a6b36e0e0d))
* set proper default config values, and update the readme ([#15](https://github.com/doublewordai/control-layer/issues/15)) ([2d9f5e6](https://github.com/doublewordai/control-layer/commit/2d9f5e64690b97a73c673d71118a1d7ebcaf79f9))
* update demos to match all current features ([#21](https://github.com/doublewordai/control-layer/issues/21)) ([83b5886](https://github.com/doublewordai/control-layer/commit/83b5886b32287a1db86c424b2d320cd07a979ffe))

## [0.1.3](https://github.com/doublewordai/control-layer/compare/v0.1.2...v0.1.3) (2025-10-17)


### Bug Fixes

* update demos to match all current features ([#21](https://github.com/doublewordai/control-layer/issues/21)) ([83b5886](https://github.com/doublewordai/control-layer/commit/83b5886b32287a1db86c424b2d320cd07a979ffe))

## [0.1.2](https://github.com/doublewordai/control-layer/compare/v0.1.1...v0.1.2) (2025-10-16)


### Bug Fixes

* docs change to trigger release please ([#18](https://github.com/doublewordai/control-layer/issues/18)) ([8d2ae51](https://github.com/doublewordai/control-layer/commit/8d2ae51be6b26b01300c9a3484c484a6b36e0e0d))

## [0.1.1](https://github.com/doublewordai/control-layer/compare/v0.1.0...v0.1.1) (2025-10-15)


### Bug Fixes

* set proper default config values, and update the readme ([#15](https://github.com/doublewordai/control-layer/issues/15)) ([2d9f5e6](https://github.com/doublewordai/control-layer/commit/2d9f5e64690b97a73c673d71118a1d7ebcaf79f9))

## 0.1.0 (2025-10-15)

### Features

* trigger release please ([95a195b](https://github.com/doublewordai/control-layer/commit/95a195bf677a6c09114a23a08e60a28143e112f6))

### Bug Fixes

* better OSS ux, bundle DB, frontend into single binary,  rename to waycast, simplify CI([#6](https://github.com/doublewordai/control-layer/issues/6)) ([dd4bfa3](https://github.com/doublewordai/control-layer/commit/dd4bfa3b3d012be33055402805a317b3a7e7766a))
