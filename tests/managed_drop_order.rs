// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every drop order of the four managed session parts must complete promptly.

#![cfg(all(windows, any(feature = "blocking", feature = "tokio")))]

pub mod helpers;

use std::time::{Duration, Instant};

const ORDERS: [[usize; 4]; 24] = [
    [0, 1, 2, 3],
    [0, 1, 3, 2],
    [0, 2, 1, 3],
    [0, 2, 3, 1],
    [0, 3, 1, 2],
    [0, 3, 2, 1],
    [1, 0, 2, 3],
    [1, 0, 3, 2],
    [1, 2, 0, 3],
    [1, 2, 3, 0],
    [1, 3, 0, 2],
    [1, 3, 2, 0],
    [2, 0, 1, 3],
    [2, 0, 3, 1],
    [2, 1, 0, 3],
    [2, 1, 3, 0],
    [2, 3, 0, 1],
    [2, 3, 1, 0],
    [3, 0, 1, 2],
    [3, 0, 2, 1],
    [3, 1, 0, 2],
    [3, 1, 2, 0],
    [3, 2, 0, 1],
    [3, 2, 1, 0],
];

const DROP_BUDGET: Duration = Duration::from_secs(30);
const WATCHDOG_BUDGET: Duration = Duration::from_secs(60);

#[cfg(feature = "blocking")]
#[test]
fn blocking_managed_parts_complete_in_every_drop_order() {
    use conpty_oxide::blocking::Command;

    helpers::with_timeout(WATCHDOG_BUDGET, || {
        let started = Instant::now();
        for order in ORDERS {
            let parts = Command::new("cmd.exe")
                .args(["/d", "/c", "pause"])
                .spawn()
                .expect("managed spawning must succeed")
                .into_parts();
            let mut child = Some(parts.child);
            let mut output = Some(parts.output);
            let mut input = Some(parts.input);
            let mut controller = Some(parts.controller);

            for part in order {
                match part {
                    0 => drop(child.take()),
                    1 => drop(output.take()),
                    2 => drop(input.take()),
                    3 => drop(controller.take()),
                    _ => unreachable!(),
                }
            }
        }
        assert!(
            started.elapsed() < DROP_BUDGET,
            "blocking managed-part teardown exceeded {DROP_BUDGET:?}"
        );
    });
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn tokio_managed_parts_complete_in_every_drop_order() {
    use conpty_oxide::tokio::Command;

    let _watchdog = helpers::watchdog(WATCHDOG_BUDGET);
    let started = Instant::now();
    for order in ORDERS {
        let parts = Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .spawn()
            .expect("managed spawning must succeed")
            .into_parts();
        let mut child = Some(parts.child);
        let mut output = Some(parts.output);
        let mut input = Some(parts.input);
        let mut controller = Some(parts.controller);

        for part in order {
            match part {
                0 => drop(child.take()),
                1 => drop(output.take()),
                2 => drop(input.take()),
                3 => drop(controller.take()),
                _ => unreachable!(),
            }
        }
    }
    assert!(
        started.elapsed() < DROP_BUDGET,
        "Tokio managed-part teardown exceeded {DROP_BUDGET:?}"
    );
}
