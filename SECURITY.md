# Security policy

## Supported versions

tierstore is at its first public release line. Only the latest release receives security fixes. A fix ships as a new
release from `main`; there are no backports to older releases. Run the newest release: the
[install guide](crates/tierstore/docs/install.md) describes moving to a new release.

## Reporting a vulnerability

Report privately. Do not open a public issue, pull request, or discussion for a vulnerability.

Use GitHub private vulnerability reporting: open the
[Security tab](https://github.com/komira-ai/tierstore/security/advisories/new) of `komira-ai/tierstore` and choose
"Report a vulnerability". Only the maintainers can read the report.

Include what you can of:

- the release version (`tierstore --version`) and the platform;
- the configuration involved (remove key material first);
- the steps that reproduce the problem, and what you expected instead;
- the impact you see, for example reading or writing another role's objects, bypassing the signature check, or running
  code as a privileged account through the update hook.

## What is in scope

- The S3 listener: request signing and presigned URLs, key roles and their grants, and request parsing.
- Key and configuration handling, including key files and their permissions.
- The update hook, which runs as `tierstore-update` with `CAP_NET_ADMIN` on Linux and as root on macOS, and the firewall (nftables) and launchd changes it makes.
- The release artifacts and their attestations.

Reports about a deployment's own misconfiguration (for example a key file readable by every user, or a listener bound
to a public address) are welcome as documentation fixes through a normal issue.

## What to expect

These are targets for the maintainers, not guarantees:

1. We acknowledge a report within 5 business days.
2. We confirm or dismiss it, and tell you which, within 14 days of the acknowledgement.
3. We prepare a fix and a release, and keep you informed while we do. We aim to publish within 90 days of the report,
   sooner for a severe problem.
4. When the fix is released we publish a GitHub security advisory that describes the problem, the affected releases, and
   the fixed release. We credit the reporter unless asked not to.

If a fix needs more than 90 days we agree a new date with you before the first date passes. Please keep the report
private until the advisory is published.
