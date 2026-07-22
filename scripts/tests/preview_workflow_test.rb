# frozen_string_literal: true

require "minitest/autorun"

class PreviewWorkflowTest < Minitest::Test
  ROOT = File.expand_path("../..", __dir__)

  def test_preview_publication_is_bound_to_the_authoritative_pr_head
    workflow = File.read(File.join(ROOT, ".github", "workflows", "ci.yaml"))

    assert_includes workflow, "github.event.pull_request.head.sha"
    assert_includes workflow, "DOCKER_METADATA_SHORT_SHA_LENGTH: 40"
    assert_includes workflow, "steps.preview-image.outputs.digest"
    assert_includes workflow, "needs: [workflow-contract, frontend-test, backend-test, build]"
    assert_includes workflow, "name: Publish preview artifact"
    assert_includes workflow, "startsWith(github.head_ref, 'preview/')"
    assert_includes workflow, "github.event.pull_request.head.repo.full_name == github.repository"
    assert_includes workflow, "preview-artifact-published"
    assert_includes workflow, "artifact_kind: 'image'"
    assert_includes workflow, "artifact_repository: 'ghcr.io/doublewordai/control-layer'"
    refute_includes workflow, "pull_request_target"
    refute_includes workflow, "repo: 'internal'"
    refute_includes workflow, "preview-*.doubleword.ai"
  end

  def test_closing_a_preview_pr_publishes_only_its_source_identity
    workflow = File.read(File.join(ROOT, ".github", "workflows", "preview-close.yml"))

    assert_includes workflow, "types: [closed]"
    assert_includes workflow, "startsWith(github.head_ref, 'preview/')"
    assert_includes workflow, "github.event.pull_request.head.repo.full_name == github.repository"
    assert_includes workflow, "preview-closed"
    assert_includes workflow, "context.payload.pull_request.head.sha"
    refute_includes workflow, "artifact_kind"
    refute_includes workflow, "pull_request_target"
    refute_includes workflow, "doubleword.ai"
  end

  def test_preview_workflows_pin_reusable_actions
    ci_workflow = File.read(File.join(ROOT, ".github", "workflows", "ci.yaml"))
    close_workflow = File.read(File.join(ROOT, ".github", "workflows", "preview-close.yml"))
    workflow_contract = ci_workflow[/^  workflow-contract:\n.*?(?=^  \S)/m]
    preview_publish = ci_workflow[/^  preview:\n.*?(?=^  \S)/m]

    refute_nil workflow_contract
    refute_nil preview_publish
    assert_includes workflow_contract,
                    "actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803 # v6"

    github_script = "actions/github-script@3a2844b7e9c422d3c10d287c895573f7108da1b3 # v9"
    assert_includes preview_publish, github_script
    assert_includes close_workflow, github_script

    [workflow_contract, preview_publish, close_workflow].each do |workflow|
      workflow.scan(/uses:\s+\S+@(\S+)/).each do |(reference)|
        assert_match(/\A[0-9a-f]{40}\z/, reference)
      end
    end
  end

  def test_comment_driven_staging_build_is_removed
    refute File.exist?(File.join(ROOT, ".github", "workflows", "build-staging.yml"))
  end
end
