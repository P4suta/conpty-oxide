# conpty-oxide

Correctness-first Windows ConPTY (pseudoconsole) library with sync and async (tokio) APIs.

## Goals

- **EOF contract** — reading the console output pipe ends with a clean,
  well-defined EOF once the session is over; no lost trailing output and no
  reads that block forever.
- **No `ClosePseudoConsole` hangs** — the close/drain ordering that ConPTY
  requires is handled by the library, so callers never deadlock on teardown.
- **Kill tree via Job objects** — the spawned process and all of its
  descendants are placed in a Job object so the whole tree can be terminated
  reliably.
- **Sync + tokio** — a blocking API by default (`blocking` feature) and an
  async API behind the `tokio` feature.
- **Dynamic `conpty.dll` loading** — prefer a modern `conpty.dll` when
  available and fall back to the system console API otherwise.

## Status

Work in progress. APIs are not stable yet; expect breaking changes before 1.0.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
