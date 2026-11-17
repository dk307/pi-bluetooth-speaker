# Release and Versioning Process

The workspace ships as one appliance release. Every member inherits the canonical version from
`[workspace.package].version` in the root `Cargo.toml`; do not add package-local version literals.

## Automatic releases

`release-plz` runs from `.github/workflows/release.yml` after every push to `main`:

1. A qualifying commit causes release-plz to open or update a `chore: release vX.Y.Z` pull request.
2. That pull request updates the root workspace version and `Cargo.lock`, and adds the release notes
   to `CHANGELOG.md`.
3. Merging the release pull request creates the `vX.Y.Z` git tag and publishes the GitHub release.
4. The same merge runs the normal ARM64 CI workflow, so binaries built from the released source
   report the released version through `--version`.

The repository uses git-only releases. Nothing is published to crates.io. `release-plz.toml` disables
per-package tags and releases, with `screen-manager` as the single release owner, so the appliance
produces exactly one `vX.Y.Z` tag. `release_always = false` prevents an ordinary push from creating
a release; only merging the release pull request can do so.

## Repository setup

The workflow needs the repository to allow `GITHUB_TOKEN` to create pull requests. In
**Settings → Actions → General → Workflow permissions**, enable **Allow GitHub Actions to create and
approve pull requests**. Without it, the `Prepare Next Release` job fails with HTTP 403 and the
message `GitHub Actions is not permitted to create or approve pull requests`, even though the job
declares `pull-requests: write`.

The same setting is managed through the REST API:

```bash
gh api --method PUT repos/OWNER/REPO/actions/permissions/workflow \
  -f default_workflow_permissions=read \
  -F can_approve_pull_request_reviews=true
```

## Commit types

Release pull requests are created only for these conventional commit types:

| Commit type | Version change |
| --- | --- |
| `feat:` | Minor |
| `fix:`, `perf:`, `refactor:`, `revert:` | Patch |
| Breaking change (`!` or `BREAKING CHANGE`) | Major |
| `chore:`, `docs:`, `test:`, `build:` | No release |

A scoped conventional commit such as `fix(ui): ...` is also accepted. The release pull request's
`chore:` merge commit is intentionally excluded, preventing an endless release-pull-request loop.

## Local preview

Install the pinned release tool and preview the files it would update:

```bash
cargo install release-plz --version 0.4.3 --locked
release-plz update --allow-dirty
git diff -- Cargo.toml Cargo.lock CHANGELOG.md
```

The first automated release is `v2.1.0`; earlier development commits did not have release tags.
