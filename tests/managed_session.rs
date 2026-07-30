// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Managed-session operations must be symmetric across both front ends.

#![cfg(all(windows, any(feature = "blocking", feature = "tokio")))]

pub mod helpers;

use conpty_oxide::{SessionOptions, Size};

fn size(rows: u16, cols: u16) -> Size {
    Size::try_new(rows, cols).expect("test dimensions must be valid")
}

#[cfg(feature = "blocking")]
mod blocking {
    use conpty_oxide::blocking::{Command, Session};
    use conpty_oxide::ConPtyBackend;
    use std::io::{Read, Write};

    use super::*;

    const fn assert_io<T: Read + Write>() {}

    #[test]
    fn shared_options_and_output_expose_their_documented_values() {
        let backend = ConPtyBackend::system().expect("the system ConPTY backend must be available");
        let options = SessionOptions::new().size(size(29, 91)).backend(backend);

        let session = Command::new("cmd.exe")
            .args(["/d", "/c", "echo", "shared-api-output"])
            .spawn_with(options)
            .expect("managed spawning with explicit options must succeed");
        assert_eq!(session.size(), size(29, 91));
        let output = session
            .wait_with_output()
            .expect("managed output collection must succeed");

        assert!(output.status().success());
        assert!(output
            .as_bytes()
            .windows(b"shared-api-output".len())
            .any(|window| window == b"shared-api-output"));
        let rendered = format!("{output:?}");
        assert!(rendered.contains("SessionOutput"), "{rendered}");
        assert!(rendered.contains("bytes"), "{rendered}");

        let bytes = output.into_bytes();
        assert!(bytes
            .windows(b"shared-api-output".len())
            .any(|window| window == b"shared-api-output"));
    }

    #[test]
    fn exposes_io_control_poll_and_kill_operations() {
        assert_io::<Session>();

        let mut session = Command::new("cmd.exe")
            .args(["/d", "/c", "ping", "-t", "127.0.0.1"])
            .spawn_with(SessionOptions::new().size(size(20, 70)))
            .expect("managed spawning must succeed");

        assert_eq!(session.size(), size(20, 70));
        assert!(
            session
                .try_wait()
                .expect("polling the child must succeed")
                .is_none(),
            "the infinite ping fixture must still be running"
        );
        session
            .resize(size(30, 100))
            .expect("resizing a live managed session must succeed");
        assert_eq!(session.size(), size(30, 100));
        assert_eq!(
            session.clear().is_ok(),
            session.supports_clear(),
            "the managed capability query and operation must agree"
        );
        session
            .kill()
            .expect("killing the managed tree must succeed");
        let output = session
            .wait_with_output()
            .expect("a killed managed session must drain and finish");
        assert_eq!(output.status().code(), 1);
    }

    #[test]
    fn cloned_controller_survives_the_original_and_controls_owned_halves() {
        let parts = Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .spawn()
            .expect("managed spawning must succeed")
            .into_parts();
        let controller = parts.controller.clone();
        drop(parts.controller);

        controller
            .resize(size(31, 101))
            .expect("a cloned controller must keep the session controllable");
        assert_eq!(controller.size(), size(31, 101));

        drop(parts.child);
        drop(parts.output);
        drop(parts.input);
        drop(controller);
    }
}

#[cfg(feature = "tokio")]
mod asynchronous {
    use tokio::io::{AsyncRead, AsyncWrite};

    use conpty_oxide::tokio::{Command, Session};

    use super::*;

    const fn assert_io<T: AsyncRead + AsyncWrite + Unpin>() {}

    #[tokio::test]
    async fn exposes_io_control_poll_and_kill_operations() {
        assert_io::<Session>();

        let mut session = Command::new("cmd.exe")
            .args(["/d", "/c", "ping", "-t", "127.0.0.1"])
            .spawn_with(SessionOptions::new().size(size(20, 70)))
            .expect("managed spawning must succeed");

        assert_eq!(session.size(), size(20, 70));
        assert!(
            session
                .try_wait()
                .expect("polling the child must succeed")
                .is_none(),
            "the infinite ping fixture must still be running"
        );
        session
            .resize(size(30, 100))
            .expect("resizing a live managed session must succeed");
        assert_eq!(session.size(), size(30, 100));
        assert_eq!(
            session.clear().is_ok(),
            session.supports_clear(),
            "the managed capability query and operation must agree"
        );
        session
            .kill()
            .expect("killing the managed tree must succeed");
        let output = session
            .wait_with_output()
            .await
            .expect("a killed managed session must drain and finish");
        assert_eq!(output.status().code(), 1);
    }

    #[tokio::test]
    async fn cloned_controller_survives_the_original_and_controls_owned_halves() {
        let parts = Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .spawn()
            .expect("managed spawning must succeed")
            .into_parts();
        let controller = parts.controller.clone();
        drop(parts.controller);

        controller
            .resize(size(31, 101))
            .expect("a cloned controller must keep the session controllable");
        assert_eq!(controller.size(), size(31, 101));

        drop(parts.child);
        drop(parts.output);
        drop(parts.input);
        drop(controller);
    }
}
