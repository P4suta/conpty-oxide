// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pseudoconsole dimensions.
//!
//! [`Size`] is a validated pair of terminal dimensions. `ConPTY` represents the
//! console size as a `COORD` whose members are `i16`, so each dimension must
//! be in `1..=`[`Size::MAX_DIMENSION`]. This module is pure Rust and has no
//! dependency on `windows-sys`.

use core::fmt;

use crate::error::{Error, Result};

/// Dimensions of a pseudoconsole, in character cells.
///
/// A `Size` is always valid: both dimensions are non-zero and at most
/// [`Size::MAX_DIMENSION`]. Construct one with [`Size::try_new`];
/// [`Size::default`] is 24 rows by 80 columns.
///
/// # Examples
///
/// ```
/// use conpty_oxide::Size;
///
/// # fn main() -> conpty_oxide::Result<()> {
/// let size = Size::try_new(24, 80)?;
/// assert_eq!(size.rows(), 24);
/// assert_eq!(size.cols(), 80);
/// assert_eq!(size, Size::default());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    rows: u16,
    cols: u16,
}

impl Size {
    /// Maximum value for either dimension: `i16::MAX` (32767).
    ///
    /// `ConPTY`'s `COORD` stores dimensions as `i16`, so anything larger cannot
    /// be represented.
    pub const MAX_DIMENSION: u16 = i16::MAX as u16;

    /// Creates a `Size`, validating both dimensions.
    ///
    /// # Errors
    ///
    /// Returns an error with [`crate::ErrorKind::InvalidSize`] if either
    /// dimension is `0` or greater than [`Size::MAX_DIMENSION`].
    pub const fn try_new(rows: u16, cols: u16) -> Result<Self> {
        if rows == 0 || cols == 0 || rows > Self::MAX_DIMENSION || cols > Self::MAX_DIMENSION {
            return Err(Error::invalid_size(rows, cols));
        }
        Ok(Self { rows, cols })
    }

    /// Returns the number of rows (screen buffer height).
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Returns the number of columns (screen buffer width).
    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// Returns `(rows, cols)` as `i16`, in that order, for building a
    /// `ConPTY` `COORD` (`COORD.Y` = rows, `COORD.X` = cols).
    ///
    /// The conversion cannot truncate: both dimensions are guaranteed to be
    /// at most [`Size::MAX_DIMENSION`] (`i16::MAX`).
    ///
    /// Takes `self` by value because `Size` is `Copy`
    /// (`clippy::wrong_self_convention`).
    #[must_use]
    #[cfg(any(feature = "blocking", feature = "tokio", test))]
    pub(super) const fn to_i16_pair(self) -> (i16, i16) {
        (
            i16::from_ne_bytes(self.rows.to_ne_bytes()),
            i16::from_ne_bytes(self.cols.to_ne_bytes()),
        )
    }
}

/// 24 rows by 80 columns, the traditional terminal size.
impl Default for Size {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// Formats as `<cols>x<rows>` — columns first, matching the conventional
/// terminal geometry notation (e.g. the default size displays as `80x24`).
impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

/// Constructs a hard-coded valid size for crate-local tests.
#[cfg(test)]
pub(super) fn test_size(rows: u16, cols: u16) -> Size {
    Size::try_new(rows, cols).expect("the hard-coded test size is valid")
}

#[cfg(test)]
#[path = "size_tests.rs"]
mod tests;
