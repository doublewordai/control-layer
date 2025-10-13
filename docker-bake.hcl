# Docker Bake file for building control-layer images with multiplatform support and attestations

variable "REGISTRY" {
  default = "ghcr.io/doublewordai/control-layer"
}

# Build platforms
variable "PLATFORMS" {
  default = "linux/amd64,linux/arm64"
}

# Global tags (comma-separated)
variable "TAGS" {
  default = ""
}

# Enable/disable attestations (provenance and SBOM)
variable "ATTESTATIONS" {
  default = "true"
}

# Clay main application (includes frontend)
target "clay" {
  context = "."
  dockerfile = "Dockerfile"
  tags = TAGS != "" ? [for tag in split(",", TAGS) : "${REGISTRY}/clay:${tag}"] : []
  labels = {}
  platforms = split(",", PLATFORMS)
  annotations = []
  attest = ATTESTATIONS == "true" ? [
    "type=provenance,mode=max",
    "type=sbom"
  ] : []
}

# Group target for building all images
group "default" {
  targets = ["clay"]
}
