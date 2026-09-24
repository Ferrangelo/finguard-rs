//! Command-line entry point for the expense-date repair tool.
//!
//! Usage:
//!   cargo run --bin repair_expense_dates
//!   cargo run --bin repair_expense_dates -- --apply --day=first-of-month
//!
//! With no flags this runs a dry run: it reports, per affected file, how
//! many rows are missing a date (class A) or carry a date outside their own
//! file's month (class B), and writes nothing. This is the file-list audit
//! to read and approve before ever adding `--apply`.
//!
//! `--apply` writes the repair, and requires `--day` alongside it, since the
//! day within a repaired month cannot be recovered from any column; there
//! is no default. See `finguard_rs_backend::expense_date_repair` for the
//! full contract, including which rows are a defect and which are not.

use finguard_rs_backend::expense_date_repair::{DayPolicy, repair_expense_dates};

fn usage() -> &'static str {
    "Usage: repair_expense_dates [--apply --day=first-of-month]\n\n\
     With no flags: dry run, reports counts and file names only, writes nothing.\n\
     --apply: write the repair. Requires --day.\n\
     --day=<policy>: the day assigned to a repaired row's month. Only 'first-of-month' \
     is implemented today."
}

fn main() {
    let mut apply = false;
    let mut day = None;

    for arg in std::env::args().skip(1) {
        if arg == "--apply" {
            apply = true;
        } else if arg == "--help" || arg == "-h" {
            println!("{}", usage());
            return;
        } else if let Some(value) = arg.strip_prefix("--day=") {
            match DayPolicy::parse(value) {
                Some(parsed) => day = Some(parsed),
                None => {
                    eprintln!(
                        "repair_expense_dates: unknown --day value '{value}'. The only policy \
                         implemented today is 'first-of-month'.\n\n{}",
                        usage()
                    );
                    std::process::exit(2);
                }
            }
        } else {
            eprintln!(
                "repair_expense_dates: unrecognized argument '{arg}'.\n\n{}",
                usage()
            );
            std::process::exit(2);
        }
    }

    match repair_expense_dates(apply, day) {
        Ok(report) => println!("{report}"),
        Err(err) => {
            eprintln!("repair_expense_dates: {err}");
            std::process::exit(1);
        }
    }
}
