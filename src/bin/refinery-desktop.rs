//! The `refinery-desktop` executable: a native window over the local daemon.

fn main() -> std::process::ExitCode {
    match refinery::desktop::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let mut text = format!("refinery-desktop: {error}");
            if let Some(source) = std::error::Error::source(&error) {
                text.push_str(&format!("\n  caused by: {source}"));
            }
            eprintln!("{text}");
            // A window that failed to open still has a way to say why.
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Refinery could not start")
                .set_description(&text)
                .show();
            std::process::ExitCode::FAILURE
        }
    }
}
