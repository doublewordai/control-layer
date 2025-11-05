# Changelog

## [0.6.0](https://github.com/doublewordai/control-layer/compare/v0.5.1...v0.6.0) (2025-11-05)


### Features

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
