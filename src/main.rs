use std::path::PathBuf;

use jotter::ui::run;

/// Locate the tray icon.
///
/// Inside a .app the working directory is not the repo root, so a bare
/// relative path fails — and the app must run from the bundle, since that is
/// the only way macOS will grant it audio permissions. Prefer the bundle's
/// Resources directory and fall back to the repo layout for `cargo run`.
fn icon_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        // .../Jotter.app/Contents/MacOS/jotter -> .../Contents/Resources/icon.png
        if let Some(contents) = exe.parent().and_then(|p| p.parent()) {
            let bundled = contents.join("Resources/icon.png");
            if bundled.is_file() {
                return bundled;
            }
        }
    }
    PathBuf::from("assets/icon.png")
}

fn main() -> eframe::Result<()> {
    let mut native_options = eframe::NativeOptions::default();
    native_options.viewport = native_options
        .viewport
        .clone()
        // Shown on launch: the app has a Dock icon (LSUIElement is false), and
        // a Dock icon that bounces into nothing visible reads as a failed
        // launch. Closing the window hides it; the tray reopens it.
        .with_visible(true)
        .with_inner_size([420.0, 380.0]);

    run(&icon_path(), native_options)
}
