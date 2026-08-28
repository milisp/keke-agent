//! Handing a URL to whatever the desktop considers a browser.
//!
//! Every OAuth surface needs this and none of them should own it: the terminal
//! had a copy, the alternate-screen interface had none at all, and a person
//! signing in from the interface was left to copy a URL by hand. Spawning a
//! browser writes nothing to the terminal, so it is safe from under a full-screen
//! draw loop — only *printing* the URL is not, which is why showing it stays the
//! caller's job.

/// Ask the desktop to open `url`, ignoring whether it managed to.
///
/// Callers must show the URL as well: a failure here is silent by design —
/// there is no browser on a headless box and nothing useful to say about it.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    spawn(url)
}

#[cfg(target_os = "macos")]
fn spawn(url: &str) -> std::io::Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(drop)
}

#[cfg(target_os = "linux")]
fn spawn(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(drop)
}

#[cfg(target_os = "windows")]
fn spawn(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(drop)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn spawn(_url: &str) -> std::io::Result<()> {
    Ok(())
}
