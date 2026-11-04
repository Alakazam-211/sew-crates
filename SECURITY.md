# Security policy

## Reporting a vulnerability

Please report security problems **privately**. Do not open a public issue, pull request or discussion.

Use GitHub private vulnerability reporting: the **Security** tab of this repository → **Report a vulnerability**.

Include the crate name and version, what you found, and how to reproduce it if you can. A minimal reproduction is the most useful thing you can send.

## What happens next

1. We acknowledge your report within **3 business days**.
2. The team that maintains the affected crate confirms the problem and works on a fix. We keep you informed, and may ask you questions.
3. The fix is prepared **privately, under embargo**, by the team that maintains the crate. No public branch, tag, pull request or release carries an unpublished fix. Downstream projects that need the fix early carry it as a private vendored patch, with no network access and no credentials, and remove it once the fixed version is on crates.io.
4. We agree a disclosure date with you. Our default is **90 days** from your report, or sooner once the fix is ready. Nothing is disclosed before a fix is ready, unless the problem is already being exploited in public.
5. On that date the fixed release and its advisory publish **together**: a GitHub Security Advisory and a [RustSec](https://rustsec.org) advisory. The fixed version is never published ahead of its advisory. Affected versions are yanked when that is safe for users.
6. We credit you in the advisory, unless you would rather stay anonymous.

## For users of these crates

- Pin exact versions and commit your `Cargo.lock`, so a release reaches you only when you choose to take it.
- Formats, protocols and documented error codes are API under semver: a breaking change to any of them is a major version.
- Watch [RustSec](https://rustsec.org) or this repository's security advisories for announcements.

## Supported versions

Before 1.0, only the latest published minor version receives security fixes.

| Version | Supported |
|---|---|
| latest `0.x` | yes |
| older | no |
