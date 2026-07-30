// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

#[test]
fn try_new_accepts_boundary_values() {
    let min = Size::try_new(1, 1).expect("1x1 must be valid");
    assert_eq!((min.rows(), min.cols()), (1, 1));

    let max =
        Size::try_new(Size::MAX_DIMENSION, Size::MAX_DIMENSION).expect("32767x32767 must be valid");
    assert_eq!((max.rows(), max.cols()), (32767, 32767));
}

#[test]
fn try_new_rejects_zero() {
    let rows = Size::try_new(80, 0).expect_err("zero rows must fail");
    assert_eq!(rows.kind(), crate::ErrorKind::InvalidSize);
    assert!(rows.to_string().contains("0 rows x 80 cols"));

    let cols = Size::try_new(0, 24).expect_err("zero columns must fail");
    assert_eq!(cols.kind(), crate::ErrorKind::InvalidSize);
    assert!(cols.to_string().contains("24 rows x 0 cols"));
}

#[test]
fn try_new_rejects_values_above_max() {
    let rows = Size::try_new(80, 32768).expect_err("oversized rows must fail");
    assert_eq!(rows.kind(), crate::ErrorKind::InvalidSize);
    let cols = Size::try_new(32768, 24).expect_err("oversized columns must fail");
    assert_eq!(cols.kind(), crate::ErrorKind::InvalidSize);
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
    assert_eq!(
        Size::try_new(132, 50)
            .expect("the hard-coded size is valid")
            .to_string(),
        "132x50"
    );
}

#[test]
fn try_new_and_to_i16_pair_are_columns_then_rows() {
    let size = Size::try_new(80, 24).expect("80 columns by 24 rows must be valid");
    assert_eq!(size.cols(), 80);
    assert_eq!(size.rows(), 24);
    assert_eq!(size.to_string(), "80x24");
    assert_eq!(size.to_i16_pair(), (80, 24));
    assert_eq!(
        Size::try_new(1, Size::MAX_DIMENSION)
            .expect("the hard-coded size is valid")
            .to_i16_pair(),
        (1, i16::MAX)
    );
}
