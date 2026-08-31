//! Run the versioned offline refinement-quality evaluation set.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let mut root = PathBuf::from("evaluations/v1");
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--set" => {
                let Some(value) = arguments.next() else {
                    return usage("--set needs a directory");
                };
                root = PathBuf::from(value);
            }
            "--json" => json = true,
            other => return usage(&format!("unexpected argument {other}")),
        }
    }

    let report = match refinery::evaluations::run(&root) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("refinery-eval: {error}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("report is serializable")
        );
    } else {
        println!("Refinery evaluation set {}", report.set_version);
        println!("case                         self  faith ground explicit questions overall");
        for case in &report.cases {
            println!(
                "{:<28} {:>4.2}  {:>4.2}  {:>4.2}    {:>4.2}      {:>4.2}    {:>4.2}",
                case.id,
                case.self_containment,
                case.faithfulness,
                case.grounding,
                case.explicitness,
                case.question_economy,
                case.overall,
            );
        }
        println!(
            "average {:.2} (minimum {:.2}; per-case minimum {:.2}) — {}",
            report.average,
            report.minimum_average,
            report.minimum_case_score,
            if report.passed { "PASS" } else { "FAIL" }
        );
    }
    if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn usage(error: &str) -> ExitCode {
    eprintln!("refinery-eval: {error}");
    eprintln!("usage: refinery-eval [--set <directory>] [--json]");
    ExitCode::FAILURE
}
