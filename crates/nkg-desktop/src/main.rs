#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod binary_template;
mod edit;
mod json;
mod theme;
mod xml;

use app::NkgApp;
use eframe::egui;

fn main() -> eframe::Result {
    let mut arguments = std::env::args_os().skip(1);
    let initial_path = arguments.next().map(std::path::PathBuf::from);
    let second_argument = arguments.next();
    let (initial_compare_path, initial_search) =
        if second_argument.as_deref() == Some(std::ffi::OsStr::new("--search")) {
            (
                None,
                arguments.next().map(|value| value.to_string_lossy().into()),
            )
        } else {
            (second_argument.map(std::path::PathBuf::from), None)
        };
    let app_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/nkg-icon.png"))
        .expect("embedded application icon must be a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NKG Uni Text Edit")
            .with_icon(app_icon)
            .with_decorations(false)
            .with_resizable(true)
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 560.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "nkg-uni-text-edit",
        options,
        Box::new(move |context| {
            Ok(Box::new(NkgApp::new(
                context,
                initial_path,
                initial_compare_path,
                initial_search,
            )))
        }),
    )
}
