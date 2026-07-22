'use strict'

const assert = require('node:assert/strict')
const {
  shouldPublishFloatingTags,
} = require('./onwards-floating-tags.cjs')

function release(tagName, overrides = {}) {
  return {
    tag_name: tagName,
    draft: false,
    prerelease: false,
    ...overrides,
  }
}

const releases = [
  release('v8.94.0'),
  release('onwards-v0.35.3'),
  release('onwards-v0.35.4'),
  release('onwards-v0.35.5'),
  release('onwards-v0.36.0-rc.1', { prerelease: true }),
  release('onwards-v99.0.0', { draft: true }),
]

assert.equal(
  shouldPublishFloatingTags('onwards-v0.35.5', releases),
  true,
  'the newest stable Onwards release should refresh floating tags',
)
assert.equal(
  shouldPublishFloatingTags('onwards-v0.35.4', releases),
  false,
  'rerunning an older release must not roll floating tags backward',
)
assert.equal(
  shouldPublishFloatingTags('onwards-v0.36.0-rc.1', releases),
  false,
  'prereleases must not receive stable floating tags',
)
assert.equal(
  shouldPublishFloatingTags('onwards-v99.0.0', releases),
  false,
  'draft releases must not receive floating tags',
)
assert.equal(
  shouldPublishFloatingTags('onwards-v0.35.6', releases),
  false,
  'an unpublished tag must not receive floating tags',
)

assert.equal(
  shouldPublishFloatingTags('onwards-v10.0.0', [
    release('onwards-v9.99.99'),
    release('onwards-v10.0.0'),
  ]),
  true,
  'versions must be compared numerically rather than lexicographically',
)

assert.equal(
  shouldPublishFloatingTags('onwards-v9007199254740993.0.0', [
    release('onwards-v9007199254740992.999.999'),
    release('onwards-v9007199254740993.0.0'),
  ]),
  true,
  'version comparison must remain exact above Number.MAX_SAFE_INTEGER',
)

console.log('Onwards floating image tag tests passed')
