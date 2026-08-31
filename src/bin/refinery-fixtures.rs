//! Check or record sanitized provider fixtures.
//!
//! Record mode is intentionally awkward to enter: a caller must set
//! `REFINERY_RECORD_FIXTURES=1`, and the raw capture is parsed and scrubbed in
//! memory before the destination file is opened.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use refinery::diagnostics::fixtures;

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("refinery-fixtures: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<String>) -> Result<String, String> {
    match arguments.as_slice() {
        [command, root] if command == "check" => check(Path::new(root)),
        [command, input, output] if command == "record" => {
            record(Path::new(input), Path::new(output))
        }
        _ => Err(format!(
            "usage:\n  refinery-fixtures check <file-or-directory>\n  \
             {}=1 refinery-fixtures record <input-or--> <output>",
            fixtures::RECORD_ENV
        )),
    }
}

fn check(root: &Path) -> Result<String, String> {
    let files = json_files(root)?;
    if files.is_empty() {
        return Err(format!("{} contains no JSON fixtures", root.display()));
    }
    for path in &files {
        let input = std::fs::read_to_string(path)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        if !fixtures::is_sanitized(&input)? {
            return Err(format!(
                "{} contains credential-shaped data; re-record it through the sanitized recorder",
                path.display()
            ));
        }
    }
    Ok(format!("{} recorded fixtures are sanitized", files.len()))
}

fn record(input: &Path, output: &Path) -> Result<String, String> {
    if !fixtures::record_mode_enabled() {
        return Err(format!(
            "record mode is disabled; set {}=1 deliberately",
            fixtures::RECORD_ENV
        ));
    }
    fixtures::register_recording_secrets();
    let raw = if input == Path::new("-") {
        let mut raw = String::new();
        std::io::stdin()
            .read_to_string(&mut raw)
            .map_err(|error| format!("reading stdin: {error}"))?;
        raw
    } else {
        std::fs::read_to_string(input)
            .map_err(|error| format!("reading {}: {error}", input.display()))?
    };
    let sanitized = fixtures::sanitize_document(&raw)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    std::fs::write(output, sanitized)
        .map_err(|error| format!("writing {}: {error}", output.display()))?;
    Ok(format!("recorded sanitized fixture {}", output.display()))
}

fn json_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if root.is_file() {
        return Ok(
            (root.extension().and_then(|value| value.to_str()) == Some("json"))
                .then(|| root.to_path_buf())
                .into_iter()
                .collect(),
        );
    }
    if !root.is_dir() {
        return Err(format!("{} does not exist", root.display()));
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("reading {}: {error}", directory.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|error| format!("reading {}: {error}", directory.display()))?
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("json") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}
