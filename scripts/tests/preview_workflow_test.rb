# frozen_string_literal: true

require "minitest/autorun"

class PreviewWorkflowTest < Minitest::Test
  ROOT = File.expand_path("../..", __dir__)

  def test_preview_publication_is_bound_to_the_authoritative_pr_head
    workflow = File.read(File.join(ROOT, ".github", "workflows", "ci.yaml"))
    backend_gate = workflow[/^  backend-test:\n.*?(?=^  \S)/m]

    refute_nil backend_gate
    assert_includes workflow, "github.event.pull_request.head.sha"
    assert_includes workflow, "DOCKER_METADATA_SHORT_SHA_LENGTH: 40"
    assert_includes workflow, "steps.preview-image.outputs.digest"
    assert_includes backend_gate,
                    "needs: [backend-crate-test, backend-dwctl-test, backend-lint, frontend-test, build]"
    assert_includes backend_gate, "ruby scripts/tests/preview_workflow_test.rb"
    assert_includes backend_gate, "name: Publish preview artifact"
    assert_includes backend_gate, "startsWith(github.head_ref, 'preview/')"
    assert_includes backend_gate,
                    "github.event.pull_request.head.repo.full_name == github.repository"
    assert_includes backend_gate, "needs.frontend-test.result == 'success'"
    assert_includes backend_gate, "needs.build.result == 'success'"
    assert_includes backend_gate, "preview-artifact-published"
    assert_includes backend_gate, "artifact_kind: 'image'"
    assert_includes backend_gate,
                    "artifact_repository: 'ghcr.io/doublewordai/control-layer'"
    refute_match(/^  workflow-contract:/, workflow)
    refute_match(/^  preview:/, workflow)
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
    backend_gate = ci_workflow[/^  backend-test:\n.*?(?=^  \S)/m]

    refute_nil backend_gate

    github_script = "actions/github-script@3a2844b7e9c422d3c10d287c895573f7108da1b3 # v9"
    assert_includes backend_gate, github_script
    assert_includes close_workflow, github_script

    [backend_gate, close_workflow].each do |workflow|
      workflow.scan(%r{uses:\s+actions/github-script@(\S+)}).each do |(reference)|
        assert_match(/\A[0-9a-f]{40}\z/, reference)
      end
    end
  end

  def test_dispatches_share_the_preview_token_and_opaque_deploy_target
    %w[ci.yaml preview-close.yml release.yml].each do |filename|
      workflow = File.read(File.join(ROOT, ".github", "workflows", filename))

      assert_includes workflow, "github-token: ${{ secrets.PREVIEW_DISPATCH_TOKEN }}"
      assert_includes workflow, "owner: '${{ secrets.DEPLOY_TARGET_OWNER }}'"
      assert_includes workflow, "repo: '${{ secrets.DEPLOY_TARGET_REPO }}'"
      refute_includes workflow, "PREVIEW_DISPATCH_OWNER"
      refute_includes workflow, "PREVIEW_DISPATCH_REPOSITORY"
      refute_includes workflow, "DEPLOY_PAT"
    end

    release_workflow = File.read(File.join(ROOT, ".github", "workflows", "release.yml"))
    deployment_dispatch = release_workflow[/^  notify-deploy:\n.*?(?=^  \S)/m]
    refute_nil deployment_dispatch
    assert_includes deployment_dispatch,
                    "uses: actions/github-script@3a2844b7e9c422d3c10d287c895573f7108da1b3 # v9"
    refute_includes deployment_dispatch, "uses: actions/github-script@v9"
  end

  def test_comment_driven_staging_build_is_removed
    refute File.exist?(File.join(ROOT, ".github", "workflows", "build-staging.yml"))
  end
end
