'use strict'

const stableOnwardsTag = /^onwards-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/

function parseStableOnwardsVersion(tagName) {
  const match = stableOnwardsTag.exec(tagName)
  if (match === null) {
    return null
  }

  return match.slice(1).map((component) => BigInt(component))
}

function compareVersions(left, right) {
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] < right[index]) {
      return -1
    }
    if (left[index] > right[index]) {
      return 1
    }
  }
  return 0
}

function shouldPublishFloatingTags(releaseTag, releases) {
  if (!Array.isArray(releases)) {
    throw new TypeError('GitHub releases must be an array')
  }

  const targetVersion = parseStableOnwardsVersion(releaseTag)
  if (targetVersion === null) {
    return false
  }

  const stableReleases = releases
    .filter((release) => !release.draft && !release.prerelease)
    .map((release) => ({
      tagName: release.tag_name,
      version: parseStableOnwardsVersion(release.tag_name),
    }))
    .filter((release) => release.version !== null)

  if (!stableReleases.some((release) => release.tagName === releaseTag)) {
    return false
  }

  const newestVersion = stableReleases.reduce(
    (newest, release) =>
      compareVersions(release.version, newest) > 0 ? release.version : newest,
    targetVersion,
  )

  return compareVersions(targetVersion, newestVersion) === 0
}

module.exports = {
  shouldPublishFloatingTags,
}
