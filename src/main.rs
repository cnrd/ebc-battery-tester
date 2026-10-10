#![warn(clippy::all, rust_2018_idioms)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

// When compiling natively:
#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([400.0, 300.0])
        .with_min_inner_size([300.0, 220.0]);
    // NOTE: Adding an icon is optional
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(&include_bytes!("../assets/favicon-512x512.png")[..])
    {
        viewport = viewport.with_icon(icon);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "eframe template",
        native_options,
        Box::new(|cc| Ok(Box::new(ebc_battery_tester::MainApp::new(cc)))),
    )
}

// When compiling to web using trunk:
#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;

    // Redirect `log` message to `console.log` and friends:
    eframe::WebLogger::init(log::LevelFilter::Debug).ok();

    let web_options = eframe::WebOptions::default();

    wasm_bindgen_futures::spawn_local(async {
        let Some(window) = web_sys::window() else {
            log::error!("No browser window is available");
            return;
        };
        let Some(document) = window.document() else {
            log::error!("No browser document is available");
            return;
        };

        let Some(canvas) = document.get_element_by_id("the_canvas_id") else {
            log::error!("Failed to find the_canvas_id");
            return;
        };
        let Ok(canvas) = canvas.dyn_into::<web_sys::HtmlCanvasElement>() else {
            log::error!("the_canvas_id was not a HtmlCanvasElement");
            return;
        };

        let start_result = eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| Ok(Box::new(ebc_battery_tester::MainApp::new(cc)))),
            )
            .await;

        // Remove the loading text and spinner:
        if let Some(loading_text) = document.get_element_by_id("loading_text") {
            match start_result {
                Ok(_) => {
                    loading_text.remove();
                }
                Err(e) => {
                    loading_text.set_inner_html(
                        "<p> The app has crashed. See the developer console for details. </p>",
                    );
                    log::error!("Failed to start eframe: {e:?}");
                }
            }
        }
    });
}
