---
name: inkentry-release
description: Release gates for the inkentry repo: run to prepare a release for publishing. Updates crates, documentation, and the changelog. Sets the version number in Cargo.toml. Creates a PR that once merged can be tagged to trigger the release. Use after making any change to this repository that should be released.
---

# Verify: inkentry release
All work in this skill should happen on a branch that is based on the origin/main branch. Make sure you run git fetch before branching. The branch should be on its own worktree to not interfere with other work the human may be doing on the main worktree. The release process will create a PR from this branch to main. Once the PR is merged, the release can be tagged and published.

## Determine the next version number
First determine the next version number, if a semver version was provided as part of the skills input use that. If not determine the next version number based on the current version in Cargo.toml and the type of release (major, minor, patch) you want to make. Ask the user for confirmation of the next version number before proceeding. If the changelog unreleased section has breaking changes then the next version number should be a major version. If the changelog unreleased section has new features then the next version number should be a minor version. If the changelog unreleased section has only bug fixes then the next version number must be a patch version.

## Update the dependencies
Run `cargo update` to update the dependencies in Cargo.lock. If any dependencies were updated run `cargo audit` and `cargo deny` before proceeding. Run **every gate in the repo's `.claude/skills/verify/SKILL.md`** and fix until green — that file is the authoritative gate list.

# Docs updates
In a sub agent (Suggested model: Claude Sonnet 5) run the instructions in [docs writer persona](references/docs-writer.md) to update the docs, tell the agent the existing version number and the suggested new version number. The docs update step must be completed before the version number is updated in Cargo.toml.

## Update the changelog
Update the changelog unreleased section to include the new version number and the current date. Move the unreleased section to a new section with the new version number and the current date. If there are any breaking changes in the unreleased section, add a note about the breaking changes in the new version section.

## Update the version number in Cargo.toml
This step can't happen until the docs updates step has completed.We have multiple crates in the workspace, so update the version number in each crate's Cargo.toml. Use `cargo set-version` to update the version number in each Cargo.toml file.
