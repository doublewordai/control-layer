# Changelog

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
