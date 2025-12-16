# Changelog

## [0.16.0](https://github.com/doublewordai/control-layer/compare/v0.15.1...v0.16.0) (2025-12-16)


### Features

* permissions audit ([036e77e](https://github.com/doublewordai/control-layer/commit/036e77e56bd845f3c28fdc5889a18d680e8d4bf6))
* Retry failed batch requests ([#354](https://github.com/doublewordai/control-layer/issues/354)) ([b7ed485](https://github.com/doublewordai/control-layer/commit/b7ed485eb37d3d3924b35922323c9efbe2f0e808))
* Support tariffs to set per-workload model costs ([#336](https://github.com/doublewordai/control-layer/issues/336)) ([0a16081](https://github.com/doublewordai/control-layer/commit/0a16081aa725620707060b7673a9b6b970c9550f))


### Bug Fixes

* grant credits to proxy header users ([#366](https://github.com/doublewordai/control-layer/issues/366)) ([0910efe](https://github.com/doublewordai/control-layer/commit/0910efeede12a4b56e9a46c2cbeeebe80b8a29a6))
* timeout text ([#363](https://github.com/doublewordai/control-layer/issues/363)) ([7006a6b](https://github.com/doublewordai/control-layer/commit/7006a6bc04448d6a4daec6c2f83d0514455d9b61))

## [0.15.1](https://github.com/doublewordai/control-layer/compare/v0.15.0...v0.15.1) (2025-12-12)


### Bug Fixes

* ability to specify models to seed and option to add them to everyone group ([#347](https://github.com/doublewordai/control-layer/issues/347)) ([4e7c9fd](https://github.com/doublewordai/control-layer/commit/4e7c9fd65a966294efcb173cbbef934a43bad9fa))

## [0.15.0](https://github.com/doublewordai/control-layer/compare/v0.14.0...v0.15.0) (2025-12-12)


### Features

* File cost estimates ([#341](https://github.com/doublewordai/control-layer/issues/341)) ([fa65db0](https://github.com/doublewordai/control-layer/commit/fa65db01769332a2cf607c6f1d71514e579fa69e))
* user soft delete + permissions fixes ([#346](https://github.com/doublewordai/control-layer/issues/346)) ([ee1c864](https://github.com/doublewordai/control-layer/commit/ee1c864739457fdc9d35ccba5851368043b69fbc))


### Bug Fixes

* DASHBOARD_BOOTSTRAP_JS env var ([#328](https://github.com/doublewordai/control-layer/issues/328)) ([40a995a](https://github.com/doublewordai/control-layer/commit/40a995a6d5d798afe9197165e9a41861c7f0808c))
* playground access error and model name overflow ([#340](https://github.com/doublewordai/control-layer/issues/340)) ([b99eb9a](https://github.com/doublewordai/control-layer/commit/b99eb9ab516671093c0ea1ba083d4526fb0e7c55))
* playground model selection & error enrichment ([#345](https://github.com/doublewordai/control-layer/issues/345)) ([c74296f](https://github.com/doublewordai/control-layer/commit/c74296f2441ca7fd07264f3204a257be64a35bca))

## [0.14.0](https://github.com/doublewordai/control-layer/compare/v0.13.0...v0.14.0) (2025-12-03)


### Features

* fusillade 0.6.0 ([#320](https://github.com/doublewordai/control-layer/issues/320)) ([623d3ce](https://github.com/doublewordai/control-layer/commit/623d3ce267c766ba2e77275d2255c963da0439c4))


### Bug Fixes

* cancel safety in background services ([#308](https://github.com/doublewordai/control-layer/issues/308)) ([5b0ec02](https://github.com/doublewordai/control-layer/commit/5b0ec021917b2762a1453e0ba6fcd909804ba650))
* remove batch analytics ([#316](https://github.com/doublewordai/control-layer/issues/316)) ([27d7032](https://github.com/doublewordai/control-layer/commit/27d703276924a019bc80afa75c3ef1f337a463f2))
* slow mega-query ([#310](https://github.com/doublewordai/control-layer/issues/310)) ([250652b](https://github.com/doublewordai/control-layer/commit/250652b92af5b37cf552b2c8cdc8d2fb00216e73))

## [0.13.0](https://github.com/doublewordai/control-layer/compare/v0.12.0...v0.13.0) (2025-12-02)


### Features

* Add batch analytics endpoint with request-level metrics tracking ([#304](https://github.com/doublewordai/control-layer/issues/304)) ([e43d423](https://github.com/doublewordai/control-layer/commit/e43d423317c952927ee62a67f7519df2c1351dbe))


### Bug Fixes

* **deps:** update rust crate brotli to v8 ([#297](https://github.com/doublewordai/control-layer/issues/297)) ([2f93022](https://github.com/doublewordai/control-layer/commit/2f930226418b921c775969b43ac100165208ef05))
* filetype params ([#305](https://github.com/doublewordai/control-layer/issues/305)) ([87db326](https://github.com/doublewordai/control-layer/commit/87db32679e6416ff362a1948b9e661bc8c58c672))

## [0.12.0](https://github.com/doublewordai/control-layer/compare/v0.11.1...v0.12.0) (2025-11-28)


### Features

* Paginate api keys ([#289](https://github.com/doublewordai/control-layer/issues/289)) ([772bcc1](https://github.com/doublewordai/control-layer/commit/772bcc15c835b0daa3ad9e6884aeda67db1b3d02))

## [0.11.1](https://github.com/doublewordai/control-layer/compare/v0.11.0...v0.11.1) (2025-11-28)


### Bug Fixes

* a bug in file upload where you could get incomplete utf-8 spread across chunks ([#262](https://github.com/doublewordai/control-layer/issues/262)) ([82b1238](https://github.com/doublewordai/control-layer/commit/82b1238115ad662f8c3517ad874d3093656daac4))

## [0.11.0](https://github.com/doublewordai/control-layer/compare/v0.10.1...v0.11.0) (2025-11-28)


### Features

* **deps:** bump fusillade to 0.4.0 ([#279](https://github.com/doublewordai/control-layer/issues/279)) ([b47410a](https://github.com/doublewordai/control-layer/commit/b47410afb7df7bc865a74a9638d9283151b605cb))
* make default user roles configurable via auth.default_user_roles ([#253](https://github.com/doublewordai/control-layer/issues/253)) ([f290c44](https://github.com/doublewordai/control-layer/commit/f290c44e193b155a71e440f5c83b21156a3856ed))


### Bug Fixes

* **deps:** update rust crate prometheus to 0.14 ([#273](https://github.com/doublewordai/control-layer/issues/273)) ([7dc6fb0](https://github.com/doublewordai/control-layer/commit/7dc6fb00c4a73193f8a0c57034bf1a737b9bcd83))

## [0.10.1](https://github.com/doublewordai/control-layer/compare/v0.10.0...v0.10.1) (2025-11-27)


### Bug Fixes

* decimal precision bug ([#246](https://github.com/doublewordai/control-layer/issues/246)) ([8e3062f](https://github.com/doublewordai/control-layer/commit/8e3062f571a4cbcc60f7387641307194fc2f0802))

## [0.10.0](https://github.com/doublewordai/control-layer/compare/v0.9.0...v0.10.0) (2025-11-26)


### Features

* Add include=endpoints support to resolve hosted_on references in backend ([#244](https://github.com/doublewordai/control-layer/issues/244)) ([e258dc3](https://github.com/doublewordai/control-layer/commit/e258dc33aa01aebf012dc999f95e3340a8501484))
* make Argon2 parameters configurable for faster test execution ([#239](https://github.com/doublewordai/control-layer/issues/239)) ([65c1de9](https://github.com/doublewordai/control-layer/commit/65c1de9ee261d48720f8ebad37c90768d3772cb2))


### Bug Fixes

* dont truncate billing ([#227](https://github.com/doublewordai/control-layer/issues/227)) ([d4d5040](https://github.com/doublewordai/control-layer/commit/d4d50404b08ff86a890b3c58e309c5a21dfb7b33))
* loosen test ([5a70dc1](https://github.com/doublewordai/control-layer/commit/5a70dc150183d2c0ab169591c2410a6996e77a07))
* make hidden key when a user first logs in, rather than when they first make a playground request ([#233](https://github.com/doublewordai/control-layer/issues/233)) ([a491f54](https://github.com/doublewordai/control-layer/commit/a491f5408d0d2d8aaa0d49607bc4fade1a11561e))
* simplify auth in serializers ([#242](https://github.com/doublewordai/control-layer/issues/242)) ([86568ee](https://github.com/doublewordai/control-layer/commit/86568ee244a04980871041cdff8164696c50f6cc))

## [0.9.0](https://github.com/doublewordai/control-layer/compare/v0.8.1...v0.9.0) (2025-11-25)


### Features

* change the email config format, and add tests for native auth ([#199](https://github.com/doublewordai/control-layer/issues/199)) ([a1861a8](https://github.com/doublewordai/control-layer/commit/a1861a8f683e3b39b235d907565c8663cf4d66c4))
* **dwctl + dashboard:** users pagination ([#207](https://github.com/doublewordai/control-layer/issues/207)) ([57fdb5c](https://github.com/doublewordai/control-layer/commit/57fdb5c30148f6b14d63eab2b4d7556153a46939))


### Bug Fixes

* better batch status transitions ([6a78562](https://github.com/doublewordai/control-layer/commit/6a7856286be4beaa04282914ab5b33766e10c36c))
* jwt only stores used id, rest of data fetched from db ([187e922](https://github.com/doublewordai/control-layer/commit/187e9226d0c079e92562dc4ae065f60141dc1c0a))
* use database NOW() for updated_at timestamps to prevent clock skew ([#212](https://github.com/doublewordai/control-layer/issues/212)) ([bfced03](https://github.com/doublewordai/control-layer/commit/bfced038e7294034293ef1435c9e400dbd6fa789))

## [0.8.1](https://github.com/doublewordai/control-layer/compare/v0.8.0...v0.8.1) (2025-11-24)


### Bug Fixes

* **dwctl:** batch file pagination ([#200](https://github.com/doublewordai/control-layer/issues/200)) ([0cd01b4](https://github.com/doublewordai/control-layer/commit/0cd01b4ef5dc2cd3ebc0788919684a80946019bd))

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
