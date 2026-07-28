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

## Development

`just lint` and `just test` run everything CI checks.

The tests that drive a bundled `conpty.dll` need one to drive, so they are
opt-in:

```powershell
just fetch-conpty  # download the pinned ConPTY package into vendor/conpty
just test-dll      # run the suite with CONPTY_OXIDE_TEST_DLL_DIR set
```

`just fetch-conpty` downloads a pinned version of the
[`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
package (MIT, published by the microsoft/terminal team), verifies its SHA-256,
and lays `conpty.dll` out next to the matching `OpenConsole.exe`. Nothing from
it is committed — `vendor/` is ignored. Without that directory the
external-backend tests note the skip on stderr and the rest of the suite runs
unchanged; CI always fetches it, so both backends are covered there.

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
