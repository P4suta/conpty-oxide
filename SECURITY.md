<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Security policy

## Supported versions

Until a newer minor line is released, the latest 0.1.x release is the
supported version. Pre-release commits on `main` receive fixes but are not a
stable distribution channel.

| Version | Supported |
| ------- | --------- |
| 0.1.x   | Yes       |
| < 0.1   | No        |

## Reporting a vulnerability

Do not disclose a suspected vulnerability in an issue, discussion, pull
request, test log, or other public channel. Use GitHub's
[private vulnerability report](https://github.com/P4suta/conpty-oxide/security/advisories/new).
If private vulnerability reporting is not available, wait for the repository
owner to enable it rather than publishing the report.

Include affected versions, Windows builds and architectures, enabled features,
impact, reproduction steps or a proof of concept, and any proposed mitigation.
Remove credentials, private paths, and unrelated personal data.

The maintainers will acknowledge a complete report, investigate it privately,
coordinate a fix and advisory when confirmed, and credit reporters who want
credit. Please allow time for supported Windows versions and both system and
standalone ConPTY backends to be tested before public disclosure.
