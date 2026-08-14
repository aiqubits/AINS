---
name: release-check
description: Verify a release before publishing it. Use when preparing a release, checking release artifacts, or validating publication readiness.
license: Apache-2.0
compatibility: Requires git and access to the release environment.
metadata:
  author: ains
  version: "1.0"
allowed-tools: Read Bash(git:*)
---

# Release check

1. Verify the repository status and intended version.
2. Run the documented release checks.
3. Report blockers before publishing.
