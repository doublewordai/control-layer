# Changelog

## [5.1.0](https://github.com/doublewordai/control-layer/compare/fusillade-core-v5.0.0...fusillade-core-v5.1.0) (2026-08-21)


### Features

* **fusillade:** own batch finalization in a daemon loop, decouple no… ([#1462](https://github.com/doublewordai/control-layer/issues/1462)) ([83066bb](https://github.com/doublewordai/control-layer/commit/83066bb951405f67569d2776ad96ca61a5c372e6))

## [5.0.0](https://github.com/doublewordai/control-layer/compare/fusillade-core-v4.1.0...fusillade-core-v5.0.0) (2026-08-05)


### ⚠ BREAKING CHANGES

* tier-grouped pending demand in one query; remove the priority-decay top-up ([#1408](https://github.com/doublewordai/control-layer/issues/1408))

### Features

* attribute batch traffic to the client that submitted it ([#1410](https://github.com/doublewordai/control-layer/issues/1410)) ([2dde662](https://github.com/doublewordai/control-layer/commit/2dde6620ced2d283d1efffa376aa3a86dc7b66c0))
* signed demand windows with trailing outcome counts on /monitoring/demand ([#1399](https://github.com/doublewordai/control-layer/issues/1399)) ([24105ac](https://github.com/doublewordai/control-layer/commit/24105ac236c855dd9e4d928ad703a6a7868abf43))
* tier-grouped pending demand in one query; remove the priority-decay top-up ([#1408](https://github.com/doublewordai/control-layer/issues/1408)) ([52fa075](https://github.com/doublewordai/control-layer/commit/52fa075cf7e0faa502e8b72af3a5cae73034c771))


### Bug Fixes

* **fusillade:** add populate-duration histogram and unfrozen-termina… ([#1413](https://github.com/doublewordai/control-layer/issues/1413)) ([081f4d5](https://github.com/doublewordai/control-layer/commit/081f4d52644c8d2ad0c53b78b2e21927db306456))

## [4.1.0](https://github.com/doublewordai/control-layer/compare/fusillade-core-v4.0.0...fusillade-core-v4.1.0) (2026-07-29)


### Features

* add spare-capacity background workload processing ([#1337](https://github.com/doublewordai/control-layer/issues/1337)) ([5fc036d](https://github.com/doublewordai/control-layer/commit/5fc036dbf531e5adf754a13d94af6dfdffa12f11))
* integrate Fusillade and Onwards into the Rust workspace ([#1325](https://github.com/doublewordai/control-layer/issues/1325)) ([6f712ba](https://github.com/doublewordai/control-layer/commit/6f712ba299eb1144d20e294b11ced08d3ef4224e))
* restore fusillade storage crate releases ([#1375](https://github.com/doublewordai/control-layer/issues/1375)) ([dce6c1f](https://github.com/doublewordai/control-layer/commit/dce6c1f54f5e9e52984a46d79bee98b44d2d8789))


### Bug Fixes

* disambiguate scaled-down backend errors from upstream failures ([#819](https://github.com/doublewordai/control-layer/issues/819)) ([d9dc31f](https://github.com/doublewordai/control-layer/commit/d9dc31f10a1d1b44c8957a8021dde6cbb6986571))
* **dwctl:** pick up onwards 0.21.0 opt-in server-side tool calling ([#888](https://github.com/doublewordai/control-layer/issues/888)) ([49494d3](https://github.com/doublewordai/control-layer/commit/49494d338166f562f528f6b59465dab71d46db69))
* hide reasoning tokens if zero ([#970](https://github.com/doublewordai/control-layer/issues/970)) ([f647cd9](https://github.com/doublewordai/control-layer/commit/f647cd9a70fc5f9ce74a8d184adf1291ceb9118d))
* trigger release for fusillade 8.1.0 claim performance fix ([#844](https://github.com/doublewordai/control-layer/issues/844)) ([b04a194](https://github.com/doublewordai/control-layer/commit/b04a19406c0af84f8080367da7829623b279b785))
* update cost names ([#963](https://github.com/doublewordai/control-layer/issues/963)) ([a671cde](https://github.com/doublewordai/control-layer/commit/a671cde8a491cecc3832b9c27e2a5ad9c810a70d))

## [4.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-core-v3.0.0...fusillade-core-v4.0.0) (2026-07-21)


### ⚠ BREAKING CHANGES

* bound concurrent request state writes ([#372](https://github.com/doublewordai/fusillade/issues/372))

### Bug Fixes

* bound concurrent request state writes ([#372](https://github.com/doublewordai/fusillade/issues/372)) ([57fbfb4](https://github.com/doublewordai/fusillade/commit/57fbfb43431a9884e7f1b753255eb5962db6f314))
* fusilalde replicas dont work well in parallel for archiving ([#374](https://github.com/doublewordai/fusillade/issues/374)) ([35c5dc2](https://github.com/doublewordai/fusillade/commit/35c5dc2bb9a19471c7f84a7d3259ec64780b6605))

## [3.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-core-v2.1.0...fusillade-core-v3.0.0) (2026-07-20)


### ⚠ BREAKING CHANGES

* bound the request upload phase with a progress watchdog ([#363](https://github.com/doublewordai/fusillade/issues/363))

### Bug Fixes

* bound the request upload phase with a progress watchdog ([#363](https://github.com/doublewordai/fusillade/issues/363)) ([dc4eaf8](https://github.com/doublewordai/fusillade/commit/dc4eaf82a0acaebfdc4e29c81d476db6432c379f))

## [2.1.0](https://github.com/doublewordai/fusillade/compare/fusillade-core-v2.0.0...fusillade-core-v2.1.0) (2026-07-17)


### Features

* batch archive moves ([#359](https://github.com/doublewordai/fusillade/issues/359)) ([10308dc](https://github.com/doublewordai/fusillade/commit/10308dc828b9fe8aada604acc892465fb9d169c0))

## [2.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-core-v1.1.1...fusillade-core-v2.0.0) (2026-07-17)


### ⚠ BREAKING CHANGES

* record real duration for synthesized realtime rows ([#347](https://github.com/doublewordai/fusillade/issues/347))

### Bug Fixes

* record real duration for synthesized realtime rows ([#347](https://github.com/doublewordai/fusillade/issues/347)) ([3e2ff39](https://github.com/doublewordai/fusillade/commit/3e2ff39423ed6b6a2f7b9761c0cecadf7f4e906d))

## [1.1.1](https://github.com/doublewordai/fusillade/compare/fusillade-core-v1.1.0...fusillade-core-v1.1.1) (2026-07-16)


### Bug Fixes

* preserve independent workspace crate versions ([#345](https://github.com/doublewordai/fusillade/issues/345)) ([80824e2](https://github.com/doublewordai/fusillade/commit/80824e26109d20f3bf22c320641e8da11c12fa3b))

## [1.1.0](https://github.com/doublewordai/fusillade/compare/fusillade-core-v1.0.0...fusillade-core-v1.1.0) (2026-07-13)


### Features

* freeze terminal batch counts on the batches row ([#329](https://github.com/doublewordai/fusillade/issues/329)) ([3e51c4c](https://github.com/doublewordai/fusillade/commit/3e51c4cf53588626fe29e8536b10a875ca484999))

## [1.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-core-v0.1.0...fusillade-core-v1.0.0) (2026-07-10)


### ⚠ BREAKING CHANGES

* split storage and daemon crates ([#323](https://github.com/doublewordai/fusillade/issues/323))
* mark streaming API as breaking. BREAKING CHANGE: removed stream field from RequestData, changed ReqwestHttpClient new signature

### feat\

* mark streaming API as breaking. BREAKING CHANGE: removed stream field from RequestData, changed ReqwestHttpClient new signature ([4220b4a](https://github.com/doublewordai/fusillade/commit/4220b4a892098564c734f23f245ac24007e7bb77))


### Features

* initial fusillade release ([4102ab7](https://github.com/doublewordai/fusillade/commit/4102ab771d991e43101e59adbd4525801924ca2b))
* split storage and daemon crates ([#323](https://github.com/doublewordai/fusillade/issues/323)) ([bd309b3](https://github.com/doublewordai/fusillade/commit/bd309b343843a5b05e1bfafa68ac091a0731a172))
* test release-please with manifest config ([081662d](https://github.com/doublewordai/fusillade/commit/081662d622397369f49f176a6a1f3c9d604d606d))


### Bug Fixes

* separate file stream aborts from fusillade errors ([b1cfb1e](https://github.com/doublewordai/fusillade/commit/b1cfb1e810bd0e8e85a5a537f59ae08db8f5e8ed))
