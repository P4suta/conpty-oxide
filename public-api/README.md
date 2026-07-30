<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Public API snapshots

The four text files in this directory are the reviewed pre-1.0 public API
contract for no frontend, blocking, Tokio, and both frontends. Regenerate them
only after reviewing the API change:

```powershell
just public-api-update
```

`just public-api` also verifies that the default API equals `blocking`, that
`tracing` changes no public item, and that implementation-dependency types do
not leak. Tokio I/O traits are the sole intentional dependency-type exception.
