// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Owned I/O halves keep a managed session alive after its controller drops.

#![cfg(all(windows, any(feature = "blocking", feature = "tokio")))]

pub mod helpers;

use std::time::Duration;

const MARKER: &str = "conpty-oxide-owned-halves-live";
const BUDGET: Duration = Duration::from_secs(30);

#[cfg(feature = "blocking")]
#[test]
fn blocking_owned_halves_remain_usable_after_controller_drop() {
    use std::io::{Read, Write};
    use std::thread;

    use conpty_oxide::blocking::Command;

    use helpers::with_timeout;

    with_timeout(BUDGET, || {
        let parts = Command::new("cmd.exe")
            .spawn()
            .expect("spawning the shell must succeed")
            .into_parts();
        let mut child = parts.child;
        let mut output = parts.output;
        let mut input = parts.input;
        drop(parts.controller);

        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            output
                .read_to_end(&mut bytes)
                .expect("reading owned output must succeed");
            bytes
        });
        input
            .write_all(format!("echo {MARKER}\r\nexit 0\r\n").as_bytes())
            .expect("writing through the owned input half must succeed");

        assert!(child.wait().expect("waiting must succeed").success());
        let rendered = String::from_utf8_lossy(
            &reader
                .join()
                .expect("the owned-half reader thread must not panic"),
        )
        .into_owned();
        assert!(rendered.contains(MARKER), "{rendered:?}");
        drop(input);
    });
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn tokio_owned_halves_remain_usable_after_controller_drop() {
    use conpty_oxide::tokio::Command;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use helpers::tokio_support::within;
    use helpers::watchdog;

    let _watchdog = watchdog(BUDGET);
    within(
        "tokio_owned_halves_remain_usable_after_controller_drop",
        BUDGET,
        async {
            let parts = Command::new("cmd.exe")
                .spawn()
                .expect("spawning the shell must succeed")
                .into_parts();
            let mut child = parts.child;
            let mut output = parts.output;
            let mut input = parts.input;
            drop(parts.controller);

            let reader = tokio::spawn(async move {
                let mut bytes = Vec::new();
                output
                    .read_to_end(&mut bytes)
                    .await
                    .expect("reading owned output must succeed");
                bytes
            });
            input
                .write_all(format!("echo {MARKER}\r\nexit 0\r\n").as_bytes())
                .await
                .expect("writing through the owned input half must succeed");

            assert!(child.wait().await.expect("waiting must succeed").success());
            let bytes = reader
                .await
                .expect("the owned-half reader task must not panic");
            let rendered = String::from_utf8_lossy(&bytes);
            assert!(rendered.contains(MARKER), "{rendered:?}");
            drop(input);
        },
    )
    .await;
}
