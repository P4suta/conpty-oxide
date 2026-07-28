//! Pseudoconsole dimensions.
//!
//! [`Size`] is a validated pair of terminal dimensions. ConPTY represents the
//! console size as a `COORD` whose members are `i16`, so each dimension must
//! be in `1..=`[`Size::MAX_DIMENSION`]. This module is pure Rust and has no
//! dependency on `windows-sys`.

use core::fmt;

use crate::error::{Error, Result};

/// Dimensions of a pseudoconsole, in character cells.
///
/// A `Size` is always valid: both dimensions are non-zero and at most
/// [`Size::MAX_DIMENSION`]. Construct one with [`Size::try_new`] (fallible)
/// or [`Size::new`] (panicking); [`Size::default`] is 24 rows by 80 columns.
///
/// # Examples
///
/// ```
/// use conpty_oxide::Size;
///
/// let size = Size::new(24, 80);
/// assert_eq!(size.rows(), 24);
/// assert_eq!(size.cols(), 80);
/// assert_eq!(size, Size::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    rows: u16,
    cols: u16,
}

impl Size {
    /// Maximum value for either dimension: `i16::MAX` (32767).
    ///
    /// ConPTY's `COORD` stores dimensions as `i16`, so anything larger cannot
    /// be represented.
    pub const MAX_DIMENSION: u16 = i16::MAX as u16;

    /// Creates a `Size`, validating both dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSize`] if either dimension is `0` or greater
    /// than [`Size::MAX_DIMENSION`].
    pub fn try_new(rows: u16, cols: u16) -> Result<Self> {
        if rows == 0 || cols == 0 || rows > Self::MAX_DIMENSION || cols > Self::MAX_DIMENSION {
            return Err(Error::InvalidSize { rows, cols });
        }
        Ok(Self { rows, cols })
    }

    /// Creates a `Size`, panicking on invalid dimensions.
    ///
    /// Prefer [`Size::try_new`] when the dimensions are not known to be valid
    /// at the call site.
    ///
    /// # Panics
    ///
    /// Panics if either dimension is `0` or greater than
    /// [`Size::MAX_DIMENSION`].
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        match Self::try_new(rows, cols) {
            Ok(size) => size,
            Err(_) => panic!(
                "invalid pseudoconsole size: {rows} rows x {cols} cols \
                 (each dimension must be 1..={})",
                Self::MAX_DIMENSION
            ),
        }
    }

    /// Returns the number of rows (screen buffer height).
    #[must_use]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Returns the number of columns (screen buffer width).
    #[must_use]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Returns `(rows, cols)` as `i16`, in that order, for building a
    /// ConPTY `COORD` (`COORD.Y` = rows, `COORD.X` = cols).
    ///
    /// The conversion cannot truncate: both dimensions are guaranteed to be
    /// at most [`Size::MAX_DIMENSION`] (`i16::MAX`).
    ///
    /// Takes `self` by value because `Size` is `Copy`
    /// (`clippy::wrong_self_convention`).
    pub(crate) fn to_i16_pair(self) -> (i16, i16) {
        (self.rows as i16, self.cols as i16)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_new_accepts_boundary_values() {
        let min = Size::try_new(1, 1).expect("1x1 must be valid");
        assert_eq!((min.rows(), min.cols()), (1, 1));

        let max = Size::try_new(Size::MAX_DIMENSION, Size::MAX_DIMENSION)
            .expect("32767x32767 must be valid");
        assert_eq!((max.rows(), max.cols()), (32767, 32767));
    }

    #[test]
    fn try_new_rejects_zero() {
        assert!(matches!(
            Size::try_new(0, 80),
            Err(Error::InvalidSize { rows: 0, cols: 80 })
        ));
        assert!(matches!(
            Size::try_new(24, 0),
            Err(Error::InvalidSize { rows: 24, cols: 0 })
        ));
    }

    #[test]
    fn try_new_rejects_values_above_max() {
        assert!(matches!(
            Size::try_new(32768, 80),
            Err(Error::InvalidSize {
                rows: 32768,
                cols: 80
            })
        ));
        assert!(matches!(
            Size::try_new(24, 32768),
            Err(Error::InvalidSize {
                rows: 24,
                cols: 32768
            })
        ));
    }

    #[test]
    fn default_is_24_rows_by_80_cols() {
        let size = Size::default();
        assert_eq!(size.rows(), 24);
        assert_eq!(size.cols(), 80);
    }

    #[test]
    fn display_is_cols_then_rows() {
        assert_eq!(Size::default().to_string(), "80x24");
        assert_eq!(Size::new(50, 132).to_string(), "132x50");
    }

    #[test]
    fn to_i16_pair_is_rows_then_cols() {
        assert_eq!(Size::new(24, 80).to_i16_pair(), (24, 80));
        assert_eq!(
            Size::new(Size::MAX_DIMENSION, 1).to_i16_pair(),
            (i16::MAX, 1)
        );
    }

    #[test]
    #[should_panic(expected = "invalid pseudoconsole size")]
    fn new_panics_on_zero_rows() {
        let _ = Size::new(0, 80);
    }

    #[test]
    #[should_panic(expected = "invalid pseudoconsole size")]
    fn new_panics_above_max() {
        let _ = Size::new(24, 32768);
    }
}
