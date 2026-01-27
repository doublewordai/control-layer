# Changelog

## [4.0.0](https://github.com/doublewordai/control-layer/compare/v3.3.1...v4.0.0) (2026-01-27)


### ⚠ BREAKING CHANGES

* File limits configuration has been reorganized.

### Features

* consolidate file limits config and add max_requests_per_file ([#617](https://github.com/doublewordai/control-layer/issues/617)) ([7bd0ee3](https://github.com/doublewordai/control-layer/commit/7bd0ee304bbb2cefb02afec85f230d824f9daf3c))


### Bug Fixes

* **deps:** update dependency lucide-react to ^0.563.0 ([#621](https://github.com/doublewordai/control-layer/issues/621)) ([52d8e8e](https://github.com/doublewordai/control-layer/commit/52d8e8ea9d5fe224c2ec9beaa5b26c91dcbd65b8))

## [3.3.1](https://github.com/doublewordai/control-layer/compare/v3.3.0...v3.3.1) (2026-01-27)


### Bug Fixes

* sync package-lock.json with package.json ([fa3c491](https://github.com/doublewordai/control-layer/commit/fa3c491a0b25227af8bc9d3efbcb9b13717b28c8))

## [3.3.0](https://github.com/doublewordai/control-layer/compare/v3.2.0...v3.3.0) (2026-01-27)


### Features

* add configurable file upload concurrency limits ([#614](https://github.com/doublewordai/control-layer/issues/614)) ([67065a3](https://github.com/doublewordai/control-layer/commit/67065a36e8deb10554a3b9cf91a176f206e90496))


### Bug Fixes

* **deps:** update dependency lucide-react to ^0.563.0 ([#579](https://github.com/doublewordai/control-layer/issues/579)) ([ff714a2](https://github.com/doublewordai/control-layer/commit/ff714a23bb68d79118cd8d4b35b0576aa1948560))

## [3.2.0](https://github.com/doublewordai/control-layer/compare/v3.1.2...v3.2.0) (2026-01-27)


### Features

* Bootstrap content ([#611](https://github.com/doublewordai/control-layer/issues/611)) ([377e22d](https://github.com/doublewordai/control-layer/commit/377e22ddb39194467012260030be4774439f62e3))
* upgrade fusillade to 2.3.0 ([#613](https://github.com/doublewordai/control-layer/issues/613)) ([28862ff](https://github.com/doublewordai/control-layer/commit/28862ff27d1c2d73eb954a9687f8b3b3755f06e9))


### Bug Fixes

* don't show errors before SLA completion  ([#606](https://github.com/doublewordai/control-layer/issues/606)) ([f76fa5f](https://github.com/doublewordai/control-layer/commit/f76fa5f570c23c65a79bf39962f521bd2fb8934b))

## [3.1.2](https://github.com/doublewordai/control-layer/compare/v3.1.1...v3.1.2) (2026-01-26)


### Bug Fixes

* remove cost estimate column from files table ([#609](https://github.com/doublewordai/control-layer/issues/609)) ([1b31dc8](https://github.com/doublewordai/control-layer/commit/1b31dc8db7903209a57342769c257c2e6644560b))

## [3.1.1](https://github.com/doublewordai/control-layer/compare/v3.1.0...v3.1.1) (2026-01-26)


### Performance Improvements

* defer files query until files tab is active on batches page ([#607](https://github.com/doublewordai/control-layer/issues/607)) ([d060328](https://github.com/doublewordai/control-layer/commit/d060328b22bf534e3e7ca35ecd9a28c955aad347))

## [3.1.0](https://github.com/doublewordai/control-layer/compare/v3.0.1...v3.1.0) (2026-01-26)


### Features

* add include=analytics to batches list endpoint ([#602](https://github.com/doublewordai/control-layer/issues/602)) ([36f5ff1](https://github.com/doublewordai/control-layer/commit/36f5ff153ce5c21beba52d73d7dee89c595218b7))

## [3.0.1](https://github.com/doublewordai/control-layer/compare/v3.0.0...v3.0.1) (2026-01-26)


### Bug Fixes

* use eager connection for schema pools to respect min_connections ([#600](https://github.com/doublewordai/control-layer/issues/600)) ([ac96ba0](https://github.com/doublewordai/control-layer/commit/ac96ba05fd32b4ba4e83efa3b53a164a8673ff49))

## [3.0.0](https://github.com/doublewordai/control-layer/compare/v2.9.2...v3.0.0) (2026-01-26)


### ⚠ BREAKING CHANGES

* add runtime config for AI API base URL ([#595](https://github.com/doublewordai/control-layer/issues/595))

### Features

* add pool exhaustion error handling with 503 response ([#597](https://github.com/doublewordai/control-layer/issues/597)) ([80230ac](https://github.com/doublewordai/control-layer/commit/80230ac868b7d964a711f27bdbfc811124d2c388))
* add runtime config for AI API base URL ([#595](https://github.com/doublewordai/control-layer/issues/595)) ([1bda2ff](https://github.com/doublewordai/control-layer/commit/1bda2ff519ee9fb4f29280fdee62df68cdfa4f3b))
* decouple analytics/billing from request logging ([#596](https://github.com/doublewordai/control-layer/issues/596)) ([7846dac](https://github.com/doublewordai/control-layer/commit/7846dac137088ab51aa32a181c3dd7d3fe65e19e))


### Bug Fixes

* remove super-slow log statement in transaction writing ([#599](https://github.com/doublewordai/control-layer/issues/599)) ([443d0f4](https://github.com/doublewordai/control-layer/commit/443d0f46071b2092ea5c8d2875441f0a4defa2c5))

## [2.9.2](https://github.com/doublewordai/control-layer/compare/v2.9.1...v2.9.2) (2026-01-24)


### Bug Fixes

* use get file from primary pool to avoid any internal lag and errors ([#591](https://github.com/doublewordai/control-layer/issues/591)) ([cfa56ef](https://github.com/doublewordai/control-layer/commit/cfa56ef9d8136304cff5205a5c4644dac2961009))

## [2.9.1](https://github.com/doublewordai/control-layer/compare/v2.9.0...v2.9.1) (2026-01-23)


### Bug Fixes

* write pool for get file right after creation, due to tx race conditions ([#588](https://github.com/doublewordai/control-layer/issues/588)) ([3a2b35f](https://github.com/doublewordai/control-layer/commit/3a2b35f4e94f1a77cbf310150d6ac0c24ab02723))

## [2.9.0](https://github.com/doublewordai/control-layer/compare/v2.8.0...v2.9.0) (2026-01-23)


### Features

* add model filtering by group ([#586](https://github.com/doublewordai/control-layer/issues/586)) ([0be0ef3](https://github.com/doublewordai/control-layer/commit/0be0ef336ef0bc5019cbc66fbcf8b92f2ce7b28e))

## [2.8.0](https://github.com/doublewordai/control-layer/compare/v2.7.2...v2.8.0) (2026-01-23)


### Features

* batched inserts in fusillade ([#583](https://github.com/doublewordai/control-layer/issues/583)) ([98bc430](https://github.com/doublewordai/control-layer/commit/98bc430855aada8351226203a2ab11c935868d41))
* speed up tests and simplify database handling, also add read and write pool for outlet ([#580](https://github.com/doublewordai/control-layer/issues/580)) ([51f25af](https://github.com/doublewordai/control-layer/commit/51f25aff0d14575c60867b67582eade80e902e9b))

## [2.7.2](https://github.com/doublewordai/control-layer/compare/v2.7.1...v2.7.2) (2026-01-22)


### Bug Fixes

* handle search path at connection time rather then after ([aa9c796](https://github.com/doublewordai/control-layer/commit/aa9c79648fc5083ee56dacacb679b615dfc059b5))
* make test go zoom, remove sleep behavior in as many unit tests as possible ([#575](https://github.com/doublewordai/control-layer/issues/575)) ([d0adb91](https://github.com/doublewordai/control-layer/commit/d0adb91d8b97d359d50eca1e328aac80f28c20f5))

## [2.7.1](https://github.com/doublewordai/control-layer/compare/v2.7.0...v2.7.1) (2026-01-21)


### Bug Fixes

* revert onwards sync to use main pool due to listen notify ([032b58c](https://github.com/doublewordai/control-layer/commit/032b58cab7afc1ebe65b64e4026722a58aeb54bf))

## [2.7.0](https://github.com/doublewordai/control-layer/compare/v2.6.0...v2.7.0) (2026-01-21)


### Features

* implement read and write connections across handlers and tests ([#569](https://github.com/doublewordai/control-layer/issues/569)) ([405f33d](https://github.com/doublewordai/control-layer/commit/405f33df0c1343979be41e566addb85b7b6710cb))

## [2.6.0](https://github.com/doublewordai/control-layer/compare/v2.5.2...v2.6.0) (2026-01-21)


### Features

* add is_internal, batch_metadata_request_origin columns and remove denormalized PII ([#567](https://github.com/doublewordai/control-layer/issues/567)) ([566824f](https://github.com/doublewordai/control-layer/commit/566824f30054d98621853cdf654d13694f9ef45b))


### Bug Fixes

* better column name and removed unnecessary email join ([#570](https://github.com/doublewordai/control-layer/issues/570)) ([fb8c1e2](https://github.com/doublewordai/control-layer/commit/fb8c1e24b64bb81143104c8a8f5af63bec5c2784))

## [2.5.2](https://github.com/doublewordai/control-layer/compare/v2.5.1...v2.5.2) (2026-01-21)


### Bug Fixes

* add top level replica url to help parsing ([#565](https://github.com/doublewordai/control-layer/issues/565)) ([90882a8](https://github.com/doublewordai/control-layer/commit/90882a8206732903e137337e3bbdded7f39588d5))

## [2.5.1](https://github.com/doublewordai/control-layer/compare/v2.5.0...v2.5.1) (2026-01-20)


### Bug Fixes

* added file upload UX parity to create batch modal ([#563](https://github.com/doublewordai/control-layer/issues/563)) ([c9fc647](https://github.com/doublewordai/control-layer/commit/c9fc64761e14c9f2c50600a6a663ee883229aee7))

## [2.5.0](https://github.com/doublewordai/control-layer/compare/v2.4.2...v2.5.0) (2026-01-20)


### Features

* add replicas to schema database mode and optional parameters to set both replicas independently ([#561](https://github.com/doublewordai/control-layer/issues/561)) ([0cefcfc](https://github.com/doublewordai/control-layer/commit/0cefcfcb130a5af718baa0a02c856b3f2bd34423))

## [2.4.2](https://github.com/doublewordai/control-layer/compare/v2.4.1...v2.4.2) (2026-01-20)


### Bug Fixes

* UX bug responses still showing error ([#557](https://github.com/doublewordai/control-layer/issues/557)) ([a61b6fc](https://github.com/doublewordai/control-layer/commit/a61b6fc23c449ff923a7e46959f2fa290cfd0d07))

## [2.4.1](https://github.com/doublewordai/control-layer/compare/v2.4.0...v2.4.1) (2026-01-19)


### Bug Fixes

* bump fusillade ([#555](https://github.com/doublewordai/control-layer/issues/555)) ([aa0fbfb](https://github.com/doublewordai/control-layer/commit/aa0fbfb4ade75eb6c27f81e2a9b05dbf6bae0a10))

## [2.4.0](https://github.com/doublewordai/control-layer/compare/v2.3.0...v2.4.0) (2026-01-19)


### Features

* remove unique filename constraint on files ([#548](https://github.com/doublewordai/control-layer/issues/548)) ([2a47665](https://github.com/doublewordai/control-layer/commit/2a476658f4521802ecead5bff89bef82cf7eb72c))


### Bug Fixes

* check we'll be able to serialize custom ids at inference time ([#549](https://github.com/doublewordai/control-layer/issues/549)) ([2ba2beb](https://github.com/doublewordai/control-layer/commit/2ba2bebea075ec44c7ae48ab5874ed941324c689))

## [2.3.0](https://github.com/doublewordai/control-layer/compare/v2.2.0...v2.3.0) (2026-01-19)


### Features

* **batches:** show batch creator in platform manager UI ([#541](https://github.com/doublewordai/control-layer/issues/541)) ([bf46658](https://github.com/doublewordai/control-layer/commit/bf46658af06bb502d9f359b24aca150352d67d5d))

## [2.2.0](https://github.com/doublewordai/control-layer/compare/v2.1.1...v2.2.0) (2026-01-16)


### Features

* add sanitization response option in for models ([#542](https://github.com/doublewordai/control-layer/issues/542)) ([77e71f1](https://github.com/doublewordai/control-layer/commit/77e71f13a9519524c53a4f70448aafc011daee9c))

## [2.1.1](https://github.com/doublewordai/control-layer/compare/v2.1.0...v2.1.1) (2026-01-16)


### Bug Fixes

* use exact alias match for tariff lookup ([#544](https://github.com/doublewordai/control-layer/issues/544)) ([5a568de](https://github.com/doublewordai/control-layer/commit/5a568deab85422b1f587de51e013e593060ccd96))

## [2.1.0](https://github.com/doublewordai/control-layer/compare/v2.0.0...v2.1.0) (2026-01-15)


### Features

* trigger deployment on release ([#538](https://github.com/doublewordai/control-layer/issues/538)) ([0a43fe5](https://github.com/doublewordai/control-layer/commit/0a43fe5332b08725fe8f7e8d85d8d95aa33b71ee))


### Bug Fixes

* **dashboard:** pass is_composite filter to models API ([#536](https://github.com/doublewordai/control-layer/issues/536)) ([1ae2181](https://github.com/doublewordai/control-layer/commit/1ae2181be23913cc1710dcd03ad91d4801ec069c))

## [2.0.0](https://github.com/doublewordai/control-layer/compare/v1.3.0...v2.0.0) (2026-01-15)


### ⚠ BREAKING CHANGES

* This release includes composite/virtual models which changes the API surface for model management.

### Features

* update Cargo.lock for composite models release ([27da89b](https://github.com/doublewordai/control-layer/commit/27da89b2389cba71f140369d7bcd062c0263c2b0))


### Bug Fixes

* hide virtual model information from non-platform managers ([#534](https://github.com/doublewordai/control-layer/issues/534)) ([281ba66](https://github.com/doublewordai/control-layer/commit/281ba66f1a1e48b6cccfbae31925779f21996369))

## [1.3.0](https://github.com/doublewordai/control-layer/compare/v1.2.0...v1.3.0) (2026-01-15)


### Features

* Batch request origin in metadata and displayed in transactions history ([#530](https://github.com/doublewordai/control-layer/issues/530)) ([d8ad5a3](https://github.com/doublewordai/control-layer/commit/d8ad5a302c75ce5b110f65b63b772924724f8e51))

## [1.2.0](https://github.com/doublewordai/control-layer/compare/v1.1.2...v1.2.0) (2026-01-15)


### Features

* add composite models for weighted provider load balancing ([#532](https://github.com/doublewordai/control-layer/issues/532)) ([93cfbca](https://github.com/doublewordai/control-layer/commit/93cfbca9de7f2dc0d9817330370f3e21125a5130))
* Transaction types ([#518](https://github.com/doublewordai/control-layer/issues/518)) ([c9ddf14](https://github.com/doublewordai/control-layer/commit/c9ddf14912b168e06f5821f57ebf01cd6a849be4))


### Bug Fixes

* sum_recent_transactions_grouped includes batch_aggregates ([#531](https://github.com/doublewordai/control-layer/issues/531)) ([4a50c31](https://github.com/doublewordai/control-layer/commit/4a50c31fada2387724c88cc04326d917fb743d79))

## [1.1.2](https://github.com/doublewordai/control-layer/compare/v1.1.1...v1.1.2) (2026-01-14)


### Bug Fixes

* Billing portal support ([#526](https://github.com/doublewordai/control-layer/issues/526)) ([821845d](https://github.com/doublewordai/control-layer/commit/821845d39e76fb75bafb4d2eec29c0c9083e5979))

## [1.1.1](https://github.com/doublewordai/control-layer/compare/v1.1.0...v1.1.1) (2026-01-14)


### Bug Fixes

* icons ([#522](https://github.com/doublewordai/control-layer/issues/522)) ([2ec4914](https://github.com/doublewordai/control-layer/commit/2ec491454c2aa3242692175a3cf26d2aa6b4b1ef))
* weird stripe api restraint ([#523](https://github.com/doublewordai/control-layer/issues/523)) ([cbe7b3f](https://github.com/doublewordai/control-layer/commit/cbe7b3f06e8a0c87d05f9221eb2f7e4282f2b61e))

## [1.1.0](https://github.com/doublewordai/control-layer/compare/v1.0.0...v1.1.0) (2026-01-14)


### Features

* show both slas on model summaries ([#517](https://github.com/doublewordai/control-layer/issues/517)) ([908688c](https://github.com/doublewordai/control-layer/commit/908688c45435d7df3e5595141c94f0d86e00dbc6))


### Bug Fixes

* return to details and result count links ([#514](https://github.com/doublewordai/control-layer/issues/514)) ([e315fee](https://github.com/doublewordai/control-layer/commit/e315fee4b79f6d179065f9571df03cdd6ef7e1d9))

## [1.0.0](https://github.com/doublewordai/control-layer/compare/v0.29.0...v1.0.0) (2026-01-13)


### ⚠ BREAKING CHANGES

* move to fusillade 1.0.0 and move to model escalations [#513](https://github.com/doublewordai/control-layer/issues/513)

### Features

* move to fusillade 1.0.0 and move to model escalations [[#513](https://github.com/doublewordai/control-layer/issues/513)](https://github.com/doublewordai/control-layer/issues/513) ([61ac5e3](https://github.com/doublewordai/control-layer/commit/61ac5e35835f108a28873bc04f728a9e605cad2e))


### Bug Fixes

* move to fusillade 1.0.0 and move to model escalations ([#513](https://github.com/doublewordai/control-layer/issues/513)) ([a01e218](https://github.com/doublewordai/control-layer/commit/a01e21814113f0af6fe85822881a8e69d9b6777c))

## [0.29.0](https://github.com/doublewordai/control-layer/compare/v0.28.3...v0.29.0) (2026-01-12)


### Features

* Batch request results view ([#484](https://github.com/doublewordai/control-layer/issues/484)) ([05fcf6f](https://github.com/doublewordai/control-layer/commit/05fcf6f17a92d8c62ba82d98294e50fc27ddfd49))


### Bug Fixes

* fixed progress bar round down for batch details page also ([#507](https://github.com/doublewordai/control-layer/issues/507)) ([da1dd84](https://github.com/doublewordai/control-layer/commit/da1dd845637af2fc783a10d41c35689ff715012d))

## [0.28.3](https://github.com/doublewordai/control-layer/compare/v0.28.2...v0.28.3) (2026-01-12)


### Bug Fixes

* need to expose certain custom headers when using cors (blocked by bro… ([#505](https://github.com/doublewordai/control-layer/issues/505)) ([dfd7694](https://github.com/doublewordai/control-layer/commit/dfd76942ff5cc3163c2ceec4ce28b3ea9d86396d))

## [0.28.2](https://github.com/doublewordai/control-layer/compare/v0.28.1...v0.28.2) (2026-01-09)


### Bug Fixes

* in processign state, the progress bar pulses, now less aggrressively ([#497](https://github.com/doublewordai/control-layer/issues/497)) ([d87f24a](https://github.com/doublewordai/control-layer/commit/d87f24a84e5f80802be892264dd8d75b9a71744c))

## [0.28.1](https://github.com/doublewordai/control-layer/compare/v0.28.0...v0.28.1) (2026-01-09)


### Bug Fixes

* regenerated sqlx queries ([#495](https://github.com/doublewordai/control-layer/issues/495)) ([1c1f111](https://github.com/doublewordai/control-layer/commit/1c1f111af309cc77a736dc724afb5a12470019a8))
* various search and UI fixes ([#494](https://github.com/doublewordai/control-layer/issues/494)) ([dc99ef2](https://github.com/doublewordai/control-layer/commit/dc99ef28aa0b319616f227b8a18dd4db1f3c5cac))

## [0.28.0](https://github.com/doublewordai/control-layer/compare/v0.27.1...v0.28.0) (2026-01-09)


### Features

* sample file generators ([#468](https://github.com/doublewordai/control-layer/issues/468)) ([0c375c2](https://github.com/doublewordai/control-layer/commit/0c375c237230da8d5859595356d32610bd566672))


### Bug Fixes

* make transaction time filtering server side ([#490](https://github.com/doublewordai/control-layer/issues/490)) ([6e5928e](https://github.com/doublewordai/control-layer/commit/6e5928e8c6b13d067b0de8b0554e882d778893fe))
* removed references to expiry of files, and some UI warnings for large… ([#491](https://github.com/doublewordai/control-layer/issues/491)) ([5ff255d](https://github.com/doublewordai/control-layer/commit/5ff255d45c9169f303ec2d59d28586de67deab21))

## [0.27.1](https://github.com/doublewordai/control-layer/compare/v0.27.0...v0.27.1) (2026-01-09)


### Bug Fixes

* add histogram buckets for fusillade_retry_attempts_on_success ([#488](https://github.com/doublewordai/control-layer/issues/488)) ([2ac41f2](https://github.com/doublewordai/control-layer/commit/2ac41f2b4940eebee5df0cfa36f29f4f898fcccc))

## [0.27.0](https://github.com/doublewordai/control-layer/compare/v0.26.0...v0.27.0) (2026-01-09)


### Features

* add request_origin and batch_sla labels to gen_ai metrics and http_analytics ([#486](https://github.com/doublewordai/control-layer/issues/486)) ([e49b29e](https://github.com/doublewordai/control-layer/commit/e49b29e8feeac392f7eb37c79d83f79c51b76eb9))


### Bug Fixes

* change the ordering of prometheus initialization and background … ([#485](https://github.com/doublewordai/control-layer/issues/485)) ([d3520e1](https://github.com/doublewordai/control-layer/commit/d3520e1a0b4221bd536c06bf92735861dcddb787))
* Jansix UI fixes 2 ([#481](https://github.com/doublewordai/control-layer/issues/481)) ([ee8290c](https://github.com/doublewordai/control-layer/commit/ee8290cd532395e7706e9e829a7cbd4bcce09e6b))

## [0.26.0](https://github.com/doublewordai/control-layer/compare/v0.25.0...v0.26.0) (2026-01-08)


### Features

* improve batch modal UX with filename editing and copy updates ([#478](https://github.com/doublewordai/control-layer/issues/478)) ([a9af15d](https://github.com/doublewordai/control-layer/commit/a9af15dacc68bded996a739f81fdd794fc02e0e3))


### Bug Fixes

* round down in progress % ([#482](https://github.com/doublewordai/control-layer/issues/482)) ([ca42d89](https://github.com/doublewordai/control-layer/commit/ca42d8958e1cdad1a5adcc6bc9f45a9715950414))
* test sla e2e ([#479](https://github.com/doublewordai/control-layer/issues/479)) ([c685cd0](https://github.com/doublewordai/control-layer/commit/c685cd095cf8b7455699b4af7e4e848922999d35))

## [0.25.0](https://github.com/doublewordai/control-layer/compare/v0.24.3...v0.25.0) (2026-01-08)


### Features

* add progress bar for file uploads ([#477](https://github.com/doublewordai/control-layer/issues/477)) ([296a06a](https://github.com/doublewordai/control-layer/commit/296a06a747e94b28f1538c371b2b9dd52587a80c))


### Bug Fixes

* refresh API keys table and make HTML title configurable ([#469](https://github.com/doublewordai/control-layer/issues/469)) ([b7c4538](https://github.com/doublewordai/control-layer/commit/b7c4538ace9e7af6cb9f3197a5367b8fb5a277b2))

## [0.24.3](https://github.com/doublewordai/control-layer/compare/v0.24.2...v0.24.3) (2026-01-08)


### Bug Fixes

* when sending api requests cross origin, need to send credentials ([#474](https://github.com/doublewordai/control-layer/issues/474)) ([d6d2c3c](https://github.com/doublewordai/control-layer/commit/d6d2c3c83fbfab860c60b54b9d716f6e8426fd4c))

## [0.24.2](https://github.com/doublewordai/control-layer/compare/v0.24.1...v0.24.2) (2026-01-07)


### Bug Fixes

* when rerouting to api endpoint, strip /ai prefix to not double up ([727fe8b](https://github.com/doublewordai/control-layer/commit/727fe8bc88df503b873598c5eead61feeb56a484))

## [0.24.1](https://github.com/doublewordai/control-layer/compare/v0.24.0...v0.24.1) (2026-01-07)


### Bug Fixes

* configurable cross-origin for files and batches endpoints ([4717166](https://github.com/doublewordai/control-layer/commit/471716687d67a4a1045cf95c47ddffcff11715cf))

## [0.24.0](https://github.com/doublewordai/control-layer/compare/v0.23.0...v0.24.0) (2026-01-07)


### Features

* batch aggregation optimization for transactions endpoint ([#465](https://github.com/doublewordai/control-layer/issues/465)) ([d567568](https://github.com/doublewordai/control-layer/commit/d5675681083b5d8f76a8d16da26d7dbe1b8af89d))


### Bug Fixes

* Jansix testing fe fixes ([#464](https://github.com/doublewordai/control-layer/issues/464)) ([4492b47](https://github.com/doublewordai/control-layer/commit/4492b4748b9570c158d9e6318251bd2e7c14ce3f))

## [0.23.0](https://github.com/doublewordai/control-layer/compare/v0.22.0...v0.23.0) (2026-01-07)


### Features

* add tracing instrumentation to request serialization flow ([#459](https://github.com/doublewordai/control-layer/issues/459)) ([f8cd68a](https://github.com/doublewordai/control-layer/commit/f8cd68a9ba92b2698b3a6150d6e88d5bc308464b))
* make pool metrics sample interval configurable ([#457](https://github.com/doublewordai/control-layer/issues/457)) ([1bd23c7](https://github.com/doublewordai/control-layer/commit/1bd23c7741f06e9ce422b6d3b4629aca81db2336))


### Bug Fixes

* **deps:** update rust crate fusillade to 0.13.0 ([#462](https://github.com/doublewordai/control-layer/issues/462)) ([b6682dd](https://github.com/doublewordai/control-layer/commit/b6682dd38855660cf5c633061994e595f6e804ae))

## [0.22.0](https://github.com/doublewordai/control-layer/compare/v0.21.1...v0.22.0) (2026-01-07)


### Features

* add analytics processing lag metric ([#449](https://github.com/doublewordai/control-layer/issues/449)) ([6afa7e8](https://github.com/doublewordai/control-layer/commit/6afa7e8bdb75e170cc78e583d9441703c70525b2))
* cache sync & pool metrics ([#454](https://github.com/doublewordai/control-layer/issues/454)) ([8929ec9](https://github.com/doublewordai/control-layer/commit/8929ec965ff5a0b19de80a2fa01bd582a56aa2f2))


### Bug Fixes

* check by externalID for auth_source ([#446](https://github.com/doublewordai/control-layer/issues/446)) ([bf51e28](https://github.com/doublewordai/control-layer/commit/bf51e2893392d24ff32f09c51c5a5285b4772e5a))
* use COALESCE for checkpoint seq comparison to enable index usage ([#448](https://github.com/doublewordai/control-layer/issues/448)) ([48dcfd5](https://github.com/doublewordai/control-layer/commit/48dcfd504207a6dfb83ff1a4c8123266062ef6bc))


### Performance Improvements

* add expression index for http_analytics id::text joins ([#450](https://github.com/doublewordai/control-layer/issues/450)) ([a31f26b](https://github.com/doublewordai/control-layer/commit/a31f26bcc64d045dfb24f37d92e1461b428d53ed))

## [0.21.1](https://github.com/doublewordai/control-layer/compare/v0.21.0...v0.21.1) (2026-01-06)


### Bug Fixes

* compute balance on read instead of on write ([#445](https://github.com/doublewordai/control-layer/issues/445)) ([839373c](https://github.com/doublewordai/control-layer/commit/839373cbc1cfcd9460769649526ad045eabcdb7c))
* Prettier auth source ([#443](https://github.com/doublewordai/control-layer/issues/443)) ([e021696](https://github.com/doublewordai/control-layer/commit/e02169679ec3c1de3863e1acbb263a7d17fade58))

## [0.21.0](https://github.com/doublewordai/control-layer/compare/v0.20.0...v0.21.0) (2026-01-06)


### Features

* add --validate flag and strict config parsing ([#441](https://github.com/doublewordai/control-layer/issues/441)) ([48cc236](https://github.com/doublewordai/control-layer/commit/48cc2366be15d2a9e3caef54ce5fb234257dcf52))

## [0.20.0](https://github.com/doublewordai/control-layer/compare/v0.19.1...v0.20.0) (2026-01-06)


### Features

* support separate databases for fusillade/outlet with optional read replicas ([#433](https://github.com/doublewordai/control-layer/issues/433)) ([8c24cd0](https://github.com/doublewordai/control-layer/commit/8c24cd0d72927cc95b3dc91df5452f5a82a7a4bd))


### Bug Fixes

* **deps:** update rust crate axum-prometheus to 0.10 ([#436](https://github.com/doublewordai/control-layer/issues/436)) ([6472ebf](https://github.com/doublewordai/control-layer/commit/6472ebfdc5e16986e64e32009c465dde9dff5877))

## [0.19.1](https://github.com/doublewordai/control-layer/compare/v0.19.0...v0.19.1) (2025-12-24)


### Bug Fixes

* allow early upload in batch model for cost estimates ([#425](https://github.com/doublewordai/control-layer/issues/425)) ([24bb933](https://github.com/doublewordai/control-layer/commit/24bb9334d42336e055d9015db7bc2fc4e51dcf7b))
* hide view reuest analytics button from users without RequestViewer role ([#427](https://github.com/doublewordai/control-layer/issues/427)) ([c0469f0](https://github.com/doublewordai/control-layer/commit/c0469f0b5d83b603be0c139129431b3873595756))

## [0.19.0](https://github.com/doublewordai/control-layer/compare/v0.18.3...v0.19.0) (2025-12-22)


### Features

* allow intake of multiple SLAs ([#390](https://github.com/doublewordai/control-layer/issues/390)) ([dbe0a47](https://github.com/doublewordai/control-layer/commit/dbe0a47d173dc88bea1db43beab82d2577506802))
* Migrate analytics to http analytics table ([#416](https://github.com/doublewordai/control-layer/issues/416)) ([c5d1253](https://github.com/doublewordai/control-layer/commit/c5d12532ba18a39ab4cb288ff7ff501ce2f5b9ed))

## [0.18.3](https://github.com/doublewordai/control-layer/compare/v0.18.2...v0.18.3) (2025-12-22)


### Bug Fixes

* tidy up openapi docs ([#420](https://github.com/doublewordai/control-layer/issues/420)) ([609adf4](https://github.com/doublewordai/control-layer/commit/609adf4c725abcbfa9849d29ac8532e4e6a6fb81))

## [0.18.2](https://github.com/doublewordai/control-layer/compare/v0.18.1...v0.18.2) (2025-12-22)


### Bug Fixes

* response headers for incomplete output files didnt match the docs ([#406](https://github.com/doublewordai/control-layer/issues/406)) ([e8ea0d1](https://github.com/doublewordai/control-layer/commit/e8ea0d1f07b5326819a2fbf6a3ae1f538d9bf7cc))

## [0.18.1](https://github.com/doublewordai/control-layer/compare/v0.18.0...v0.18.1) (2025-12-20)


### Bug Fixes

* fusillade 0.11.1 ([#413](https://github.com/doublewordai/control-layer/issues/413)) ([fbc68e4](https://github.com/doublewordai/control-layer/commit/fbc68e4aafa4b9ae128d1114e584e158014b1804))

## [0.18.0](https://github.com/doublewordai/control-layer/compare/v0.17.4...v0.18.0) (2025-12-20)


### Features

* expose sla config ([#407](https://github.com/doublewordai/control-layer/issues/407)) ([bda10de](https://github.com/doublewordai/control-layer/commit/bda10de899719cbfb922494ef5e195144de97fb0))
* server-side search and filtering for users, groups, models, batches and files ([#404](https://github.com/doublewordai/control-layer/issues/404)) ([ab065a6](https://github.com/doublewordai/control-layer/commit/ab065a67472d17c1ccd84b3bbc483c18e9f24d88))


### Bug Fixes

* **deps:** update dependency lucide-react to ^0.562.0 ([#398](https://github.com/doublewordai/control-layer/issues/398)) ([105cf52](https://github.com/doublewordai/control-layer/commit/105cf5214c01e9d8bf65312c594528c154b5ae74))
* jsonl sanitization ([#405](https://github.com/doublewordai/control-layer/issues/405)) ([2dc9fef](https://github.com/doublewordai/control-layer/commit/2dc9fef72b2cd395fad5eeee5109211dc819d35b))
* make region + organization optional, remove endpoint filter or standard users ([#409](https://github.com/doublewordai/control-layer/issues/409)) ([496e7ea](https://github.com/doublewordai/control-layer/commit/496e7ea558457d7784d32492d6b31a78176eb297))
* read api key in via env var ([#408](https://github.com/doublewordai/control-layer/issues/408)) ([c13200a](https://github.com/doublewordai/control-layer/commit/c13200a120083841af4a07783fd6536c947ed986))
* Uptime and simplification for standard users ([#412](https://github.com/doublewordai/control-layer/issues/412)) ([18028e0](https://github.com/doublewordai/control-layer/commit/18028e098fccd2dac4c50125f108fda40f5e589f))

## [0.17.4](https://github.com/doublewordai/control-layer/compare/v0.17.3...v0.17.4) (2025-12-18)


### Bug Fixes

* add more permissive cors ([#400](https://github.com/doublewordai/control-layer/issues/400)) ([29003fc](https://github.com/doublewordai/control-layer/commit/29003fcc9a9319f323d655bd03cdc7b846036443))

## [0.17.3](https://github.com/doublewordai/control-layer/compare/v0.17.2...v0.17.3) (2025-12-18)


### Features

* delete batches and fix file cascade ([#396](https://github.com/doublewordai/control-layer/issues/396)) ([86b64b2](https://github.com/doublewordai/control-layer/commit/86b64b2bc088be48fe340e0d2d07efc47a2c819e))

## [0.17.2](https://github.com/doublewordai/control-layer/compare/v0.17.1...v0.17.2) (2025-12-17)


### Bug Fixes

* fusillade 0.8.2 ([#393](https://github.com/doublewordai/control-layer/issues/393)) ([3e5b9b1](https://github.com/doublewordai/control-layer/commit/3e5b9b11b52696a6ac4d71b81237452880f0e101))

## [0.17.1](https://github.com/doublewordai/control-layer/compare/v0.17.0...v0.17.1) (2025-12-17)


### Bug Fixes

* Api batch docs ([#389](https://github.com/doublewordai/control-layer/issues/389)) ([1b99045](https://github.com/doublewordai/control-layer/commit/1b990456f3a64e57cc28ad2b0afc48155a6a07b3))

## [0.17.0](https://github.com/doublewordai/control-layer/compare/v0.16.0...v0.17.0) (2025-12-17)


### Features

* dynamic Model description in model cards layout ([#368](https://github.com/doublewordai/control-layer/issues/368)) ([38adce4](https://github.com/doublewordai/control-layer/commit/38adce40b4b74e6e99ffd14e0a4f36bccbfbc28e))


### Bug Fixes

* clean up model card desc and add read more text ([#378](https://github.com/doublewordai/control-layer/issues/378)) ([7d08b3c](https://github.com/doublewordai/control-layer/commit/7d08b3c67e932fa20e9481b3ad9b38e580bdbc10))
* empty ([#384](https://github.com/doublewordai/control-layer/issues/384)) ([92dcda6](https://github.com/doublewordai/control-layer/commit/92dcda6dc65521954e824c2fad563957a88b5e1f))
* release please dashboard 3 ([#385](https://github.com/doublewordai/control-layer/issues/385)) ([1a83d70](https://github.com/doublewordai/control-layer/commit/1a83d701ee6a9ba6f1fbaafd0f85ea8a1661b8be))
* release please simple ([#387](https://github.com/doublewordai/control-layer/issues/387)) ([d52ea3c](https://github.com/doublewordai/control-layer/commit/d52ea3c6bd2d4c3ef116f0a3e9f7f9c485672a2e))
* release-please includes dashboard ([#381](https://github.com/doublewordai/control-layer/issues/381)) ([67e6f8f](https://github.com/doublewordai/control-layer/commit/67e6f8f4bf65bc343ca6257085b8646c77d184e3))
* sqlx queries ([#377](https://github.com/doublewordai/control-layer/issues/377)) ([2f0481d](https://github.com/doublewordai/control-layer/commit/2f0481d473dec09714f03ccdd8820cf8fd2f852c))
