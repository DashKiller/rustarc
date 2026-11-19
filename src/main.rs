mod app;
mod archive;
mod icons;
mod wayland_dnd;

use std::sync::{atomic::AtomicBool, mpsc::channel, Arc, Mutex};

fn main() -> eframe::Result<()> {
    // Native Wayland is the default. Drag-and-drop on Wayland is handled by
    // `wayland_dnd` (winit has none). Set RUSTARC_X11=1 to run through XWayland instead.
    if std::env::var_os("RUSTARC_X11").is_some() && std::env::var_os("DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

    // Allow `rustarc some.7z` and "Open with…" from the file manager.
    let first_arg = std::env::args().nth(1);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([560.0, 360.0])
            .with_title("RustArc")
            .with_app_id("rustarc"),
        ..Default::default()
    };

    let result = eframe::run_native(
        "RustArc",
        options,
        Box::new(move |cc| {
            let (tx, rx) = channel();
            let hovering = Arc::new(AtomicBool::new(false));
            let drag: wayland_dnd::SharedDrag = Arc::new(Mutex::new(None));
            wayland_dnd::start(cc, tx, hovering.clone(), drag.clone());
            Ok(Box::new(app::App::new(first_arg, rx, hovering, drag)))
        }),
    );
    // remove files extracted for previews / drag-out
    let _ = std::fs::remove_dir_all(app::temp_dir());
    result
}
