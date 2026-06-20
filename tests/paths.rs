//! Tests for `finguard_rs::paths`.

mod common;

use common::TestEnv;
use finguard_rs::Error;
use finguard_rs::paths::{
    get_dbs_root, get_monthly_parquet_path, get_year_dir, month_from_parquet_path,
    year_from_parquet_path, year_month_from_parquet_path,
};

#[test]
#[serial_test::serial]
fn dbs_root_is_under_xdg_data_home() {
    let env = TestEnv::new();
    let root = get_dbs_root().unwrap();
    assert_eq!(root, env.root().join("finguard").join("dbs"));
    assert!(root.is_dir(), "get_dbs_root should create the directory");
}

#[test]
#[serial_test::serial]
fn year_dir_is_created() {
    let env = TestEnv::new();
    let dir = get_year_dir(2026).unwrap();
    assert_eq!(dir, env.root().join("finguard").join("dbs").join("2026"));
    assert!(dir.is_dir());
}

#[test]
#[serial_test::serial]
fn monthly_parquet_path_filename_format() {
    let env = TestEnv::new();
    let path = get_monthly_parquet_path(2026, 3).unwrap();
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        "03_detailed_expenses.parquet"
    );
    // Parent year dir is created.
    assert!(path.parent().unwrap().is_dir());
    assert_eq!(
        path,
        env.root()
            .join("finguard")
            .join("dbs")
            .join("2026")
            .join("03_detailed_expenses.parquet")
    );
}

#[test]
#[serial_test::serial]
fn monthly_parquet_path_pads_single_digit_months() {
    let _env = TestEnv::new();
    for (month, expected) in [(1u32, "01"), (9, "09"), (10, "10"), (12, "12")] {
        let path = get_monthly_parquet_path(2026, month).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{expected}_detailed_expenses.parquet")
        );
    }
}

#[test]
#[serial_test::serial]
fn monthly_parquet_path_rejects_out_of_range_month() {
    let _env = TestEnv::new();
    for bad in [0u32, 13, 99] {
        let err = get_monthly_parquet_path(2026, bad).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)), "month {bad}");
    }
}

#[test]
fn month_from_path_round_trips() {
    for month in 1..=12u32 {
        let name = format!("{month:02}_detailed_expenses.parquet");
        let path = format!("/whatever/2026/{name}");
        assert_eq!(month_from_parquet_path(&path).unwrap(), month);
    }
}

#[test]
fn month_from_path_bad_filename() {
    let err = month_from_parquet_path("/x/2026/january.parquet").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}

#[test]
fn month_from_path_non_numeric_prefix() {
    let err = month_from_parquet_path("/x/2026/ab_detailed_expenses.parquet").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}

#[test]
fn month_from_path_out_of_range() {
    let err = month_from_parquet_path("/x/2026/13_detailed_expenses.parquet").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
    let err = month_from_parquet_path("/x/2026/00_detailed_expenses.parquet").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}

#[test]
fn year_from_path_round_trips() {
    let path = "/whatever/2026/03_detailed_expenses.parquet";
    assert_eq!(year_from_parquet_path(path).unwrap(), 2026);
}

#[test]
fn year_from_path_non_numeric_dir() {
    let err =
        year_from_parquet_path("/whatever/notayear/03_detailed_expenses.parquet").unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
}

#[test]
fn year_month_from_path_round_trips() {
    let path = "/whatever/2024/07_detailed_expenses.parquet";
    assert_eq!(year_month_from_parquet_path(path).unwrap(), (2024, 7));
}

#[test]
#[serial_test::serial]
fn env_var_override_is_respected_per_call() {
    // First env.
    let env1 = TestEnv::new();
    let root1 = get_dbs_root().unwrap();
    assert!(root1.starts_with(env1.root()));

    // Switching env (new TempDir) must change the resolved path: proves env is
    // read at call time, not cached.
    let env2 = TestEnv::new();
    let root2 = get_dbs_root().unwrap();
    assert!(root2.starts_with(env2.root()));
    assert_ne!(root1, root2);
}
