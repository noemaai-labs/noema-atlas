# Governance

Noema Atlas is maintained as an open project with public review, public issues,
and private handling only for security reports or sensitive conduct reports.

## Roles

- Contributors propose changes through issues and pull requests.
- Triagers help reproduce issues, request missing details, label reports, and
  point people to existing work.
- Maintainers review and merge pull requests, manage releases, handle security
  reports, and make final scope decisions.

Repository permissions on GitHub are the source of truth for current maintainer
and triager access.

## Decision process

Small, reversible changes can be decided in pull request review. Larger changes
should start with an issue that explains the workflow, alternatives, security
and privacy impact, and compatibility risk.

Maintainers aim for rough consensus, but the project does not require unanimity.
When a decision blocks progress, maintainers choose the option that best protects
the project's verification model, privacy defaults, cross-platform behavior, and
long-term maintainability.

## Merge policy

Pull requests should pass required CI, resolve review comments, and have at least
one maintainer approval before merge. Security-sensitive changes may need a
second maintainer review.

Maintainers may close issues or pull requests that are inactive, out of scope,
not reproducible, unsafe by default, or too broad to review. Closing a proposal
does not prevent a narrower version from being opened later.

## Releases

Releases are cut by maintainers from tags. The release process and signing
expectations are documented in [docs/releasing.md](docs/releasing.md).

## Security exceptions

Vulnerability reports are handled privately according to [SECURITY.md](SECURITY.md).
Fixes may be developed privately until disclosure would not put users at
unnecessary risk.
