use adw::glib::{self, MainContext};
use futures::future::Future;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The ID of the app.
pub static APP_ID: &str = "com.hunterwittenborn.Celeste";

/// Get the value out of a future.
pub fn await_future<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// Run a closure in the background so that the UI can keep running.
pub fn run_in_background<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(f: F) -> T {
    MainContext::default().block_on(blocking::unblock(f))
}

/// Process pending GTK events without waiting for new ones.
pub fn process_ui_events() {
    let context = MainContext::default();
    while context.pending() {
        context.iteration(false);
    }
}

/// Sleep while allowing GTK to keep processing events.
pub fn sleep_while_processing_events(duration: Duration) {
    MainContext::default().block_on(glib::timeout_future(duration));
}

/// Format a directory with the user's home directory replaced with '~'.
pub fn fmt_home(dir: &str) -> String {
    let home_dir = glib::home_dir().into_os_string().into_string().unwrap();

    match dir.strip_prefix(&home_dir) {
        Some(string) => "~".to_string() + string,
        None => dir.to_string(),
    }
}

/// Get the user's config directory.
pub fn get_config_dir() -> PathBuf {
    let mut config_dir = glib::user_config_dir();
    config_dir.push("celeste");
    config_dir
}

/// Append a diagnostic log line to Celeste's log file and stderr.
pub fn log_line(message: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| format!("{}.{:03}", duration.as_secs(), duration.subsec_millis()))
        .unwrap_or_else(|_| "unknown-time".to_string());
    let line = format!("[{now}] {message}");

    eprintln!("{line}");

    let mut log_path = get_config_dir();
    if fs::create_dir_all(&log_path).is_err() {
        return;
    }
    log_path.push("celeste.log");

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Strip the slashes from the beginning and end of a string.
pub fn strip_slashes(string: &str) -> String {
    let stripped_prefix = match string.strip_prefix('/') {
        Some(string) => string.to_string(),
        None => string.to_string(),
    };

    match stripped_prefix.strip_suffix('/') {
        Some(string) => string.to_string(),
        None => stripped_prefix,
    }
}

/// Macro to get the title of a window.
#[macro_export]
macro_rules! get_title {
    ($($arg:tt)*) => {
        tr::tr!($($arg)*) + " - Celeste"
    }
}

pub use crate::get_title;
