# Security Policy

Noema Atlas handles large model files, manifests, peer discovery, local caches,
and user credentials for private sources. Please report suspected vulnerabilities
privately so maintainers can investigate before details are public.

## Supported versions

Until the project reaches a stable 1.0 release, security fixes target `main` and
the latest published release. Maintainers may backport critical fixes when a
release is still widely used and a backport is practical.

| Version | Supported |
| --- | --- |
| `main` | Yes |
| Latest release | Yes |
| Older pre-1.0 releases | Best effort |

## Reporting a vulnerability

Use GitHub's confidential security advisory form:

https://github.com/noemaai-labs/noema-atlas/security/advisories/new

If private vulnerability reporting is not available, send the report to
clientcare@noemaai.com with `Noema Atlas security` in the subject.

Please do not open a public issue, pull request, or discussion for suspected
vulnerabilities before maintainers have had time to assess the report.

Include as much of the following as possible:

- A clear summary of the issue and affected component.
- Steps to reproduce with synthetic data where possible.
- The affected version or commit.
- Platform details.
- Expected and actual security impact.
- Logs, manifests, or proof-of-concept code with secrets removed.
- Whether the issue is already public or known to be exploited.

Do not attach proprietary model weights, private access tokens, signing keys, or
real user data. If the issue requires a large test artifact, describe how to
construct a minimal synthetic one.

## Scope

High-value areas include:

- Manifest signing, canonicalization, and verification.
- Hash verification, quarantine behavior, and source banning.
- P2P, LAN, and Iroh transport behavior.
- Tracker publication, peer discovery, and unintended data exposure.
- Local cache, install, delete, and path handling.
- Secret storage and private source credentials.
- Desktop command surfaces and webview boundaries.
- Release signing, update, and packaging integrity.
- Dependency or supply-chain vulnerabilities that affect shipped code.

Reports are usually out of scope when they require only social engineering, rely
on a compromised local administrator account, or describe denial of service from
intentionally downloading very large public files without a distinct security
boundary failure. Maintainers may still treat these as bugs.

## Response process

Maintainers aim to acknowledge new reports within two business days, then follow
up with triage status, affected versions, and a remediation plan. Timing depends
on severity and reproducibility. Coordinated disclosure is preferred; public
advisories will credit reporters who want credit.

## Safe harbor

Good-faith research is welcome when it stays within the scope above, avoids
privacy violations, avoids destructive testing, and gives maintainers reasonable
time to fix the issue before public disclosure.
