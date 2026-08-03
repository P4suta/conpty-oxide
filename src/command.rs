// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Private facade over windows-spawn's reusable command intent.

use std::ffi::OsStr;
use std::path::Path;

/// The process intent shared by the blocking and Tokio public facades.
#[derive(Debug)]
pub(super) struct Command {
    inner: windows_spawn::Command,
    #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
    kill_on_drop: bool,
}

impl Command {
    pub(super) fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            inner: windows_spawn::Command::new(program),
            #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
            kill_on_drop: false,
        }
    }

    pub(super) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub(super) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    pub(super) fn raw_arg(&mut self, text: impl AsRef<OsStr>) -> &mut Self {
        self.inner.raw_arg(text);
        self
    }

    pub(super) fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    pub(super) fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    pub(super) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    pub(super) fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    pub(super) fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
    pub(super) fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.kill_on_drop = kill;
        self
    }

    pub(super) fn get_program(&self) -> &OsStr {
        self.inner.get_program()
    }

    #[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
    pub(super) const fn get_kill_on_drop(&self) -> bool {
        self.kill_on_drop
    }

    pub(super) fn windows_spawn_mut(&mut self) -> &mut windows_spawn::Command {
        &mut self.inner
    }
}

#[cfg(all(test, any(feature = "blocking", feature = "tokio")))]
mod tests {
    use super::Command;

    #[test]
    fn records_program_and_test_drop_policy() {
        let mut command = Command::new("cmd.exe");
        command.kill_on_drop(true);
        assert_eq!(command.get_program(), "cmd.exe");
        assert!(command.get_kill_on_drop());
    }
}
