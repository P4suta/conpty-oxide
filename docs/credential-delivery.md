---
doppler_automation_app: release_plz
---

<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Credential delivery

GitHub Actions fetches Doppler credentials at their consuming step.
Each purpose uses a read-only account and config.
Shared credentials have one canonical source and explicit references.
A retrieved key retains its provider's authority.

| Purpose | Doppler config | GitHub environment variable | Environments |
| --- | --- | --- | --- |
| automation | `conpty-oxide.automation` | `DOPPLER_AUTOMATION_IDENTITY_ID` | release |

Run **Verify credential access** manually for the deployed main revision.
It checks required fields and a repository-scoped App token.
The App token grants contents read access.
The verifier performs no signing, notarization, publication, or deployment.
Existing environment protections still apply.
Approval covers only the named authentication check.

The same identity must receive an explicit denial for the unrelated config.
A network, login, or runner failure leaves the denial check inconclusive.
Workflows can exchange for every identity whose claims they satisfy.
Separate accounts with matching claims do not isolate workflow stages.

Keep existing delivery until all intended scopes and denial checks pass.
Then disconnect its Doppler sync while retaining the GitHub fields.
Check for other consumers before removing obsolete copies and configs.
Retire revoked or expired registry keys instead of migrating them.
Use native trusted publishing when the registry supports it.
Leave missing publisher bindings unconfigured until owner setup.

Record names, identity metadata, revisions, and outcomes only.
Keep credential values and native authentication state out of the repository.
