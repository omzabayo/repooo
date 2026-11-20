#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Err(error) = tabpilot::run() {
        #[cfg(windows)]
        rfd::MessageDialog::new()
            .set_title("HexBot PB MOBILE could not start")
            .set_description(error.to_string())
            .set_level(rfd::MessageLevel::Error)
            .show();
        #[cfg(not(windows))]
        eprintln!("{error}");
        std::process::exit(1);
    }
}
