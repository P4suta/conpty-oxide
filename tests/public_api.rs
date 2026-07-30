// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Positive compile checks for the supported 0.1 surface.

use std::hash::Hash;

const fn assert_kind<T: Copy + Eq + Hash>() {}

#[test]
fn stable_root_types_compile() {
    assert_kind::<conpty_oxide::ErrorKind>();
    assert_kind::<conpty_oxide::BackendErrorKind>();

    let _: Option<conpty_oxide::Error> = None;
    let _: Option<conpty_oxide::BackendError> = None;
    let _: Option<conpty_oxide::ConPtyBackend> = None;
    let _: Option<conpty_oxide::Size> = None;
    let _: Option<conpty_oxide::ExitStatus> = None;
}

#[cfg(any(feature = "blocking", feature = "tokio"))]
const fn assert_shared_controller<T: Clone + Send + Sync>() {}

#[cfg(any(feature = "blocking", feature = "tokio"))]
#[test]
fn frontend_shared_types_compile() {
    assert_shared_controller::<conpty_oxide::PtyController>();
    let _: Option<conpty_oxide::SessionOptions> = None;
    let _: Option<conpty_oxide::SessionOutput> = None;
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_surface_compiles() {
    use std::io::{Read, Write};
    use std::os::windows::io::AsHandle;

    fn command_methods(
        command: &mut conpty_oxide::blocking::Command,
        options: conpty_oxide::SessionOptions,
    ) -> conpty_oxide::Result<()> {
        command
            .arg("one")
            .args(["two"])
            .raw_arg(" three")
            .env("KEY", "VALUE")
            .envs([("OTHER", "VALUE")])
            .env_remove("KEY")
            .env_clear()
            .current_dir(".");
        let _: conpty_oxide::blocking::Session = command.spawn()?;
        let _: conpty_oxide::blocking::Session = command.spawn_with(options)?;
        Ok(())
    }

    fn collect_output(
        session: conpty_oxide::blocking::Session,
    ) -> conpty_oxide::Result<conpty_oxide::SessionOutput> {
        session.collect_output()
    }

    fn session_methods(session: &mut conpty_oxide::blocking::Session) -> conpty_oxide::Result<()> {
        let _: usize = session.read(&mut [])?;
        let _: usize = session.write(&[])?;
        let _: u32 = session.id();
        let _: Option<conpty_oxide::ExitStatus> = session.try_wait()?;
        let _: conpty_oxide::Size = session.size();
        let _: bool = session.supports_clear();
        session.resize(conpty_oxide::Size::default())?;
        let _ = session.clear();
        session.kill()
    }

    fn child_methods(child: &mut conpty_oxide::blocking::Child) -> conpty_oxide::Result<()> {
        let _ = child.as_handle();
        let _: u32 = child.id();
        let _: Option<conpty_oxide::ExitStatus> = child.try_wait()?;
        let _: conpty_oxide::ExitStatus = child.wait()?;
        child.kill()
    }

    let _ = (
        command_methods,
        collect_output,
        session_methods,
        child_methods,
    );
}

#[cfg(feature = "tokio")]
#[test]
fn tokio_surface_compiles() {
    use std::os::windows::io::AsHandle;

    fn command_methods(
        command: &mut conpty_oxide::tokio::Command,
        options: conpty_oxide::SessionOptions,
    ) -> conpty_oxide::Result<()> {
        command
            .arg("one")
            .args(["two"])
            .raw_arg(" three")
            .env("KEY", "VALUE")
            .envs([("OTHER", "VALUE")])
            .env_remove("KEY")
            .env_clear()
            .current_dir(".");
        let _: conpty_oxide::tokio::Session = command.spawn()?;
        let _: conpty_oxide::tokio::Session = command.spawn_with(options)?;
        Ok(())
    }

    fn child_methods(child: &mut conpty_oxide::tokio::Child) -> conpty_oxide::Result<()> {
        let _ = child.as_handle();
        let _: u32 = child.id();
        let _: Option<conpty_oxide::ExitStatus> = child.try_wait()?;
        child.kill()
    }

    async fn collect_output(
        session: conpty_oxide::tokio::Session,
    ) -> conpty_oxide::Result<conpty_oxide::SessionOutput> {
        session.collect_output().await
    }

    fn assert_async_io<T: ::tokio::io::AsyncRead + ::tokio::io::AsyncWrite>() {}
    assert_async_io::<conpty_oxide::tokio::Session>();
    let _ = (command_methods, collect_output, child_methods);
}
