#[cfg(windows)]
mod app;
mod automation;
mod config;
mod macros;
mod performance;
#[cfg(windows)]
mod platform;
mod storage;

pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("tabpilot=info")
        .init();
    #[cfg(windows)]
    return app::run();
    #[cfg(not(windows))]
    anyhow::bail!("HexBot PB MOBILE requires Windows and Microsoft WebView2 Runtime");
}
