# Security Policy

## Supported Versions

`TokenWeasel` is pre-1.0 and under active development. Security fixes are applied
to the latest release and the `main` branch only. There are no backported
patches for older tags.

| Version | Supported          |
| ------- | ------------------ |
| latest / `main` | :white_check_mark: |
| older tags      | :x:                |

## Reporting a Vulnerability

Repository maintainers must keep GitHub private vulnerability reporting enabled
under **Settings → Code security → Private vulnerability reporting**.

Please report security vulnerabilities **privately** through GitHub's private
vulnerability reporting: go to the repository's **Security** tab and click
**Report a vulnerability**, or use
[this link](../../security/advisories/new). This opens a private draft advisory
visible only to you and the maintainers - the details stay confidential until a
fix is published.

If private reporting is unexpectedly unavailable, open a public issue containing
only a request for a private maintainer contact. Do not include the affected
component, reproduction steps, impact, exploit details, or any other
vulnerability information in that issue.

When reporting, include as much of the following as you can:

- A clear description of the vulnerability and its impact.
- Steps to reproduce, or a minimal proof of concept.
- The affected version, commit hash, or branch.
- Any relevant configuration (with secrets redacted).

Do not disclose a vulnerability through a public issue, discussion, or pull
request. GitHub issues are public immediately.

## Handling

This is a small, best-effort project without a formal response SLA. Reports are
triaged as time allows; expect an initial acknowledgement within a reasonable
period. Once a fix is available it will land on `main` and in the next release,
and the reporter will be credited in the advisory or release notes unless they
ask otherwise.
