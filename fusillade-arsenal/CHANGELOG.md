# Changelog

## [3.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.1.4...fusillade-arsenal-v3.0.0) (2026-07-21)


### ⚠ BREAKING CHANGES

* bound concurrent request state writes ([#372](https://github.com/doublewordai/fusillade/issues/372))

### Bug Fixes

* bound concurrent request state writes ([#372](https://github.com/doublewordai/fusillade/issues/372)) ([57fbfb4](https://github.com/doublewordai/fusillade/commit/57fbfb43431a9884e7f1b753255eb5962db6f314))
* fusilalde replicas dont work well in parallel for archiving ([#374](https://github.com/doublewordai/fusillade/issues/374)) ([35c5dc2](https://github.com/doublewordai/fusillade/commit/35c5dc2bb9a19471c7f84a7d3259ec64780b6605))

## [2.1.4](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.1.3...fusillade-arsenal-v2.1.4) (2026-07-21)


### Bug Fixes

* index for batch archive sweeper for the bulk movement job, then … ([#371](https://github.com/doublewordai/fusillade/issues/371)) ([5e8f825](https://github.com/doublewordai/fusillade/commit/5e8f8259cbeb5a5fe83e1b6a254a02546450cadb))

## [2.1.3](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.1.2...fusillade-arsenal-v2.1.3) (2026-07-20)


### Bug Fixes

* synchronize workspace release dependencies ([#367](https://github.com/doublewordai/fusillade/issues/367)) ([00d3a49](https://github.com/doublewordai/fusillade/commit/00d3a49107b0a78117bc5a90260721a143618cfd))

## [2.1.2](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.1.1...fusillade-arsenal-v2.1.2) (2026-07-20)


### Bug Fixes

* migration fixes ([#364](https://github.com/doublewordai/fusillade/issues/364)) ([c0111f2](https://github.com/doublewordai/fusillade/commit/c0111f25c93ebab2e37f605bd5b5f312ed69b3d4))

## [2.1.1](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.1.0...fusillade-arsenal-v2.1.1) (2026-07-17)


### Bug Fixes

* raise internal dependency minimums to the released 2.1.0 versions ([#361](https://github.com/doublewordai/fusillade/issues/361)) ([22c4340](https://github.com/doublewordai/fusillade/commit/22c4340c4df2e55074a778e67b2193604863ee73))

## [2.1.0](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.0.1...fusillade-arsenal-v2.1.0) (2026-07-17)


### Features

* batch archive moves ([#359](https://github.com/doublewordai/fusillade/issues/359)) ([10308dc](https://github.com/doublewordai/fusillade/commit/10308dc828b9fe8aada604acc892465fb9d169c0))

## [2.0.1](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v2.0.0...fusillade-arsenal-v2.0.1) (2026-07-17)


### Bug Fixes

* resolve internal dependency requirements against the 2.0.0 major release ([#357](https://github.com/doublewordai/fusillade/issues/357)) ([0f30346](https://github.com/doublewordai/fusillade/commit/0f3034615b4abd2170e506040b8758071249fb10))

## [2.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v1.2.1...fusillade-arsenal-v2.0.0) (2026-07-17)


### ⚠ BREAKING CHANGES

* record real duration for synthesized realtime rows ([#347](https://github.com/doublewordai/fusillade/issues/347))

### Bug Fixes

* record real duration for synthesized realtime rows ([#347](https://github.com/doublewordai/fusillade/issues/347)) ([3e2ff39](https://github.com/doublewordai/fusillade/commit/3e2ff39423ed6b6a2f7b9761c0cecadf7f4e906d))

## [1.2.1](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v1.2.0...fusillade-arsenal-v1.2.1) (2026-07-17)


### Bug Fixes

* terminal request states are immutable to the daemon persist path ([#353](https://github.com/doublewordai/fusillade/issues/353)) ([86cb76c](https://github.com/doublewordai/fusillade/commit/86cb76ccfb235a49c628bfaf731ea6d86ddb01cd))

## [1.2.0](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v1.1.1...fusillade-arsenal-v1.2.0) (2026-07-16)


### Features

* add batch_requests_archive schema and routing columns (phase 3 … ([#349](https://github.com/doublewordai/fusillade/issues/349)) ([0e548d9](https://github.com/doublewordai/fusillade/commit/0e548d971fe0563e32b00dff71fc403dfc34739b))

## [1.1.1](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v1.1.0...fusillade-arsenal-v1.1.1) (2026-07-16)


### Bug Fixes

* preserve independent workspace crate versions ([#345](https://github.com/doublewordai/fusillade/issues/345)) ([80824e2](https://github.com/doublewordai/fusillade/commit/80824e26109d20f3bf22c320641e8da11c12fa3b))


### Dependencies

* The following workspace dependencies were updated
  * dependencies
    * fusillade-core bumped from 1.1.0 to 1.1.1

## [1.1.0](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v1.0.1...fusillade-arsenal-v1.1.0) (2026-07-13)


### Features

* freeze terminal batch counts on the batches row ([#329](https://github.com/doublewordai/fusillade/issues/329)) ([3e51c4c](https://github.com/doublewordai/fusillade/commit/3e51c4cf53588626fe29e8536b10a875ca484999))


### Dependencies

* The following workspace dependencies were updated
  * dependencies
    * fusillade-core bumped from 1.0.0 to 1.1.0

## [1.0.1](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v1.0.0...fusillade-arsenal-v1.0.1) (2026-07-10)


### Bug Fixes

* package sqlx metadata with storage crate ([#332](https://github.com/doublewordai/fusillade/issues/332)) ([3123839](https://github.com/doublewordai/fusillade/commit/31238392c883c8007b78e89732a4797bd16d404b))

## [1.0.0](https://github.com/doublewordai/fusillade/compare/fusillade-arsenal-v0.1.0...fusillade-arsenal-v1.0.0) (2026-07-10)


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


### Dependencies

* The following workspace dependencies were updated
  * dependencies
    * fusillade-core bumped from 0.1.0 to 1.0.0
