# Changelog

## [0.3.0](https://github.com/doublewordai/control-layer/compare/fusillade-v0.2.0...fusillade-v0.3.0) (2025-11-24)


### Features

* add actual cancellation of in-progress batch requests ([#170](https://github.com/doublewordai/control-layer/issues/170)) ([2044218](https://github.com/doublewordai/control-layer/commit/2044218ecaffbb763b1cf8750e4d072469b4be62))

## [0.2.0](https://github.com/doublewordai/control-layer/compare/fusillade-v0.1.1...fusillade-v0.2.0) (2025-11-20)


### Features

* add capacity & batch capacity ([#106](https://github.com/doublewordai/control-layer/issues/106)) ([d7f3f6d](https://github.com/doublewordai/control-layer/commit/d7f3f6d5435717dd10e2fda304bf4022a3179dc8))
* batches endpoints ([#72](https://github.com/doublewordai/control-layer/issues/72)) ([f2143c6](https://github.com/doublewordai/control-layer/commit/f2143c6be2ed1cdc1cba60e630259feb1166ab7e))
* daemon status tracking ([#96](https://github.com/doublewordai/control-layer/issues/96)) ([9222649](https://github.com/doublewordai/control-layer/commit/9222649f6706756fc5166c4747893e356f196914))
* openAI compatible files endpoints ([#60](https://github.com/doublewordai/control-layer/issues/60)) ([5c2eccd](https://github.com/doublewordai/control-layer/commit/5c2eccd3aafc8b2fabe6baadad4d26552a80da41))
* track batch status via triggers, and query in bulk rather than doing N+1 queries ([#100](https://github.com/doublewordai/control-layer/issues/100)) ([68d005d](https://github.com/doublewordai/control-layer/commit/68d005dadb00c2a4afc066b8a62c2afb528d57ef))


### Bug Fixes

* add default 30-day expiry for files when none specified ([#131](https://github.com/doublewordai/control-layer/issues/131)) ([0cce7cd](https://github.com/doublewordai/control-layer/commit/0cce7cdc266b65ed014f2ddf96b255b492d412d2)), closes [#117](https://github.com/doublewordai/control-layer/issues/117)
* better claiming logic ([#99](https://github.com/doublewordai/control-layer/issues/99)) ([a5759ff](https://github.com/doublewordai/control-layer/commit/a5759ffa1088978a6a3f575672ae3167d684a8ee))
* claim on polling interval ([#105](https://github.com/doublewordai/control-layer/issues/105)) ([2103553](https://github.com/doublewordai/control-layer/commit/2103553fa8093491fbb6f28cefe909e147b31dae))
* fewer triggers ([#103](https://github.com/doublewordai/control-layer/issues/103)) ([07f5fdf](https://github.com/doublewordai/control-layer/commit/07f5fdf5e72bbb534d175e463dbd3c8fc7f35b35))
* migrate to race-safe compaction function ([1ae9ef2](https://github.com/doublewordai/control-layer/commit/1ae9ef2656b8ce25aa537d31b0129bda68db2164))
* retries ([#102](https://github.com/doublewordai/control-layer/issues/102)) ([031c09f](https://github.com/doublewordai/control-layer/commit/031c09ffa7d109aac0eea23fb1399fc9c164972a))
* revert to aggregating batch status on demand ([#112](https://github.com/doublewordai/control-layer/issues/112)) ([04e9498](https://github.com/doublewordai/control-layer/commit/04e9498fc92e2461482f8df016c6b0e4974f0a78))
* wal for status updates ([#104](https://github.com/doublewordai/control-layer/issues/104)) ([1061b78](https://github.com/doublewordai/control-layer/commit/1061b78a4fec3c6af3dab6efb6a3a1e4a4c2c16d))

## [0.1.1](https://github.com/doublewordai/control-layer/compare/fusillade-v0.1.0...fusillade-v0.1.1) (2025-11-06)


### Bug Fixes

* add just release target, setup idempotent publishing ([3084ce1](https://github.com/doublewordai/control-layer/commit/3084ce18c95ddabc23a9716e9918dcb244e51141))

## 0.1.0 (2025-11-06)


### Features

* add fusillade: a daemon implementation for sending batched requests ([#55](https://github.com/doublewordai/control-layer/issues/55)) ([af4a60e](https://github.com/doublewordai/control-layer/commit/af4a60ed91c7e7732e6fa16427522e013b86c50b))
* trigger release please ([95a195b](https://github.com/doublewordai/control-layer/commit/95a195bf677a6c09114a23a08e60a28143e112f6))
