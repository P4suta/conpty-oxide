// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ffi::OsStr;
use std::fmt;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::path::Path;

use crate::core::child::ChildCore;
use crate::core::session;
use crate::core::wait::RegisteredWait;
use crate::error::{Error, Result};
use crate::status::ExitStatus;
use crate::{SessionOptions, SessionOutput};

use super::pty::Pty;
use super::session::Session;

/// A command to run inside an asynchronous pseudoconsole session.
///
/// Mirrors [`std::process::Command`]: the builder methods take `&mut self` and
/// return `&mut Self`, so a whole invocation can be written as one expression.
/// The differences from the standard library are the ones a pseudoconsole
/// forces:
///
/// - There is no stdio configuration. The child's console *is* the
///   pseudoconsole; its standard handles are deliberately set to
///   `INVALID_HANDLE_VALUE` so a redirected parent cannot leak its own stdio
///   into the child.
/// - No handles are inherited (`bInheritHandles` is `FALSE`), because a leaked
///   copy of the output pipe would keep the session from ever reaching
///   end-of-file.
/// - The child and every descendant it creates join a job object, which is
///   what makes [`Child::kill`] terminate the whole tree.
///
/// A command is intentionally not `Clone`: managed spawning must not copy or
/// mutate its potentially large argument and environment buffers.
///
/// ```compile_fail
/// use conpty_oxide::tokio::Command;
///
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<Command>();
/// ```
///
/// Low-level lifecycle and unvalidated process flags are intentionally absent:
///
/// ```compile_fail
/// let mut command = conpty_oxide::tokio::Command::new("cmd.exe");
/// command.creation_flags(0);
/// ```
///
/// ```compile_fail
/// let mut command = conpty_oxide::tokio::Command::new("cmd.exe");
/// command.kill_on_drop(false);
/// ```
///
/// ```compile_fail
/// let mut command = conpty_oxide::tokio::Command::new("cmd.exe");
/// command.spawn_in(());
/// ```
#[derive(Debug)]
pub struct Command {
    inner: crate::command::Command,
}

impl Command {
    /// Creates a builder for launching `program`.
    ///
    /// The program is not resolved here; a missing executable surfaces as
    /// an error with [`crate::ErrorKind::Spawn`] and a
    /// [`std::io::ErrorKind::NotFound`] source.
    #[must_use]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            inner: crate::command::Command::new(program),
        }
    }

    /// Appends one argument, quoted and escaped as the MSVC C runtime expects.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    /// Appends several arguments; equivalent to calling [`Command::arg`] for
    /// each one.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Appends literal text to the command line, bypassing all quoting.
    ///
    /// Same semantics as `std::os::windows::process::CommandExt::raw_arg`:
    /// intended for callees such as `cmd.exe /c` that parse the raw command
    /// line themselves.
    pub fn raw_arg(&mut self, text: impl AsRef<OsStr>) -> &mut Self {
        self.inner.raw_arg(text);
        self
    }

    /// Sets an environment variable for the child (case-insensitively, as
    /// Windows does).
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    /// Sets several environment variables; equivalent to calling
    /// [`Command::env`] for each pair.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    /// Removes an environment variable from the child's environment.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    /// Clears the child's environment, including modifications recorded so
    /// far. Variables set afterwards still apply.
    pub fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    /// Sets the child's working directory.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    /// Terminates the child's whole process tree when its [`Child`] is
    /// dropped. Defaults to `false`.
    ///
    /// This policy applies to [`Command::spawn_in`]. Managed
    /// [`Command::spawn`] sessions always enable kill-on-drop and
    /// kill-on-Job-close regardless of this setting.
    ///
    /// This also sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` on the session's
    /// job object, so the tree is terminated by the kernel even if this
    /// process dies without running any destructor.
    #[cfg(test)]
    pub(crate) fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.inner.kill_on_drop(kill);
        self
    }

    /// Spawns a managed asynchronous session with default options.
    ///
    /// This is synchronous because process creation itself does not block.
    /// The returned session owns a kill-on-close Job; dropping it before
    /// completion terminates the entire process tree.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend or pipes cannot be initialized, or
    /// when the root process cannot be spawned.
    pub fn spawn(&mut self) -> Result<Session> {
        self.spawn_with(SessionOptions::default())
    }

    /// Spawns a managed asynchronous session with explicit safe options.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected backend or pipes cannot be
    /// initialized, or when the root process cannot be spawned.
    pub fn spawn_with(&mut self, options: SessionOptions) -> Result<Session> {
        let (size, backend) = options.into_parts();
        let mut builder = Pty::builder().size(size);
        if let Some(backend) = backend {
            builder = builder.backend(backend);
        }
        let pty = builder.build()?;

        let child = self.spawn_in_with_policy(&pty, true)?;
        let controller = pty.controller();
        let (output, input) = pty.into_split();
        Ok(Session {
            child,
            output,
            input,
            controller,
        })
    }

    /// Runs the command in a managed session and collects its complete VT
    /// output.
    ///
    /// # Errors
    ///
    /// Returns an error if session creation, process spawning, output
    /// draining, or process waiting fails.
    pub async fn output(&mut self) -> Result<SessionOutput> {
        self.spawn()?.wait_with_output().await
    }

    /// Spawns the command as the root child of an existing low-level `pty`.
    ///
    /// Like `tokio::process::Command::spawn`, this is a synchronous method
    /// even though the resulting [`Child`] is awaited: `CreateProcessW` does
    /// not block, so there is nothing to yield for. The [`Pty`] itself must
    /// already have been built inside a Tokio runtime.
    ///
    /// The session's shutdown strategy is armed as part of this call, in the
    /// order `ConPTY` requires: the child is created, the pseudoconsole is
    /// released if the backend supports it, and only if it does not — and only
    /// when [`crate::tokio::PtyBuilder::eof_on_root_exit`] is set — is the
    /// legacy registered wait installed.
    ///
    /// A pseudoconsole hosts exactly one root child; spawning into a `Pty`
    /// that already has one fails with a
    /// [`std::io::ErrorKind::AlreadyExists`]
    /// source. (Descendants are unrestricted — the child may create as many as
    /// it likes, and they all join the same job object.)
    ///
    /// # Errors
    ///
    /// An error with [`crate::ErrorKind::Spawn`] carrying the program name and
    /// the underlying failure:
    /// [`std::io::ErrorKind::NotFound`] for a missing executable,
    /// [`std::io::ErrorKind::InvalidInput`] for a command line or environment
    /// block that cannot be built, [`std::io::ErrorKind::AlreadyExists`] for a
    /// re-used `Pty`, or the raw OS error from `CreateProcessW`.
    #[cfg(test)]
    pub(crate) fn spawn_in(&mut self, pty: &Pty) -> Result<Child> {
        self.spawn_in_with_policy(pty, self.inner.get_kill_on_drop())
    }

    fn spawn_in_with_policy(&self, pty: &Pty, kill_on_drop: bool) -> Result<Child> {
        let root = session::spawn_root(&pty.inner, &self.inner, kill_on_drop)?;
        Ok(Child {
            core: ChildCore::from_root(root),
            exit: None,
        })
    }
}

/// A running (or finished) root child of an asynchronous pseudoconsole
/// session.
///
/// The handle owns the session's job object as well as the process handle, so
/// [`Child::kill`] terminates the whole process tree rather than just the
/// process this crate created.
///
/// Dropping a `Child` does not wait for the process. A child obtained from
/// managed [`SessionParts`](super::SessionParts) terminates its process tree
/// on drop.
///
/// The process handle is available only through the lifetime-safe
/// [`AsHandle`] implementation:
///
/// ```compile_fail
/// use std::os::windows::io::AsRawHandle;
///
/// fn requires_raw_handle<T: AsRawHandle>() {}
/// requires_raw_handle::<conpty_oxide::tokio::Child>();
/// ```
pub struct Child {
    core: ChildCore,
    /// The in-flight Windows thread-pool wait. It stays in the child when a
    /// caller cancels `wait`, so a later call resumes the same registration.
    exit: Option<RegisteredWait>,
}

/// Shows the child's identity — pid, drop policy, and any cached exit status
/// — rather than the raw process and job handles, whose values are noise that
/// varies between runs.
impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.core.fmt(f)
    }
}

impl Child {
    /// Returns the child's process identifier.
    ///
    /// The identifier stays valid as long as this `Child` is alive; once the
    /// process handle is closed, Windows may reuse the number.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.core.id()
    }

    /// Waits for the child to exit and returns its status.
    ///
    /// Repeated calls return the cached status instead of waiting again. The
    /// registered wait uses only the supplied task [`Waker`](std::task::Waker);
    /// the wait itself does not occupy or require a Tokio runtime thread.
    ///
    /// # Deadlock
    ///
    /// The child must be able to make progress while this is pending, which
    /// means something else has to drain the session's output — see the module
    /// docs.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel-safe. Dropping the returned future loses no
    /// progress and no exit status: one Windows
    /// `RegisterWaitForSingleObject` registration is stored in the `Child`, so
    /// a later call resumes it. No Tokio worker or blocking-pool thread is
    /// occupied while the process is alive, and runtime shutdown is therefore
    /// independent of a pending child wait.
    ///
    /// # Errors
    ///
    /// An error with [`crate::ErrorKind::Wait`] wrapping the OS error from duplicating the process
    /// handle, `RegisterWaitForSingleObject`, or `GetExitCodeProcess`.
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.core.status() {
            return Ok(status);
        }

        if self.exit.is_none() {
            self.exit = Some(RegisteredWait::new(self.core.as_handle()).map_err(Error::wait)?);
        }
        let wait = self.exit.as_mut().ok_or_else(|| {
            Error::wait(std::io::Error::other(
                "the registered process wait was not initialized",
            ))
        })?;
        let result = wait.await;
        self.exit = None;
        let code = result.map_err(Error::wait)?;
        Ok(self.core.cache_exit_code(code))
    }

    /// Returns the exit status if the child has already exited, without
    /// waiting.
    ///
    /// A plain synchronous method: the underlying poll is a zero-timeout wait
    /// on the process handle, which never blocks.
    ///
    /// # Errors
    ///
    /// An error with [`crate::ErrorKind::Wait`] wrapping the OS error from
    /// `WaitForSingleObject` or
    /// `GetExitCodeProcess`.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let status = self.core.try_wait()?;
        if status.is_some() {
            self.exit = None;
        }
        Ok(status)
    }

    #[cfg(test)]
    pub(crate) const fn cached_status(&self) -> Option<ExitStatus> {
        self.core.status()
    }

    /// Terminates the child and every descendant it created.
    ///
    /// This terminates the session's job object, so processes the child
    /// spawned are killed too. Termination is asynchronous as far as Windows
    /// is concerned: await [`Child::wait`] afterwards to observe the resulting
    /// status, which is exit code `1`.
    ///
    /// Killing an already-finished tree succeeds and does nothing.
    ///
    /// # Errors
    ///
    /// An error with [`crate::ErrorKind::Kill`] wrapping the OS error from
    /// `TerminateJobObject`.
    pub fn kill(&mut self) -> Result<()> {
        self.core.kill()
    }
}

/// Borrows the child's process handle, e.g. to duplicate it or to wait on it
/// together with other objects.
impl AsHandle for Child {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.core.as_handle()
    }
}
