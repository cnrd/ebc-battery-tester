pub enum LogDirection {
    In,
    Out,
}

pub struct LogEntry {
    pub direction: LogDirection,
    pub label: String,
    pub timestamp: f64,
    pub raw_bytes: Vec<u8>,
}

pub fn format_log(entries: &[LogEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        let t = entry.timestamp as u64;
        let hours = t / 3600;
        let mins = (t % 3600) / 60;
        let secs = t % 60;
        let dir = match entry.direction {
            LogDirection::In => "IN ",
            LogDirection::Out => "OUT",
        };
        let hex: String = entry
            .raw_bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "{hours:02}:{mins:02}:{secs:02}  {dir}  {}\n         {hex}\n\n",
            entry.label
        ));
    }
    out
}

fn recipe_json(recipe: &crate::core::RecipeExport) -> Result<String, String> {
    serde_json::to_string_pretty(recipe)
        .map_err(|error| format!("could not encode recipe: {error}"))
}

fn recipe_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches([' ', '.']);
    format!(
        "{}.ebc-recipe.json",
        if sanitized.is_empty() {
            "recipe"
        } else {
            sanitized
        }
    )
}

fn parse_recipe(bytes: &[u8]) -> Result<crate::core::RecipeExport, String> {
    let recipe: crate::core::RecipeExport = serde_json::from_slice(bytes)
        .map_err(|error| format!("could not read recipe JSON: {error}"))?;
    recipe
        .validate()
        .map_err(|error| format!("invalid recipe: {error}"))?;
    Ok(recipe)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_recipe_from_file() -> Result<Option<crate::core::RecipeExport>, String> {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Recipe JSON", &["json"])
        .pick_file()
    else {
        return Ok(None);
    };
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    parse_recipe(&bytes).map(Some)
}

#[cfg(target_arch = "wasm32")]
pub fn load_recipe_from_file()
-> std::sync::mpsc::Receiver<Result<Option<crate::core::RecipeExport>, String>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let result = match rfd::AsyncFileDialog::new()
            .add_filter("Recipe JSON", &["json"])
            .pick_file()
            .await
        {
            Some(file) => parse_recipe(&file.read().await).map(Some),
            None => Ok(None),
        };
        let _result = sender.send(result);
    });
    receiver
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_recipe_to_file(recipe: &crate::core::RecipeExport) -> Result<(), String> {
    recipe
        .validate()
        .map_err(|error| format!("invalid recipe: {error}"))?;
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Recipe JSON", &["json"])
        .set_file_name(recipe_filename(&recipe.name))
        .save_file()
    else {
        return Ok(());
    };
    std::fs::write(&path, recipe_json(recipe)?)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

#[cfg(target_arch = "wasm32")]
pub fn save_recipe_to_file(recipe: &crate::core::RecipeExport) -> Result<(), String> {
    use wasm_bindgen::JsCast as _;

    recipe
        .validate()
        .map_err(|error| format!("invalid recipe: {error}"))?;
    let content = recipe_json(recipe)?;
    let window = web_sys::window().ok_or_else(|| "browser window is unavailable".to_owned())?;
    let document = window
        .document()
        .ok_or_else(|| "browser document is unavailable".to_owned())?;
    let array = js_sys::Array::new();
    array.push(&wasm_bindgen::JsValue::from_str(&content));
    let blob_opts = web_sys::BlobPropertyBag::new();
    blob_opts.set_type("application/json");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&array, &blob_opts)
        .map_err(|_error| "could not create recipe download".to_owned())?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|_error| "could not create recipe download URL".to_owned())?;
    let anchor = document
        .create_element("a")
        .map_err(|_error| "could not create recipe download link".to_owned())?;
    let anchor: web_sys::HtmlAnchorElement = anchor.unchecked_into();
    anchor.set_href(&url);
    anchor.set_download(&recipe_filename(&recipe.name));
    anchor.click();
    web_sys::Url::revoke_object_url(&url)
        .map_err(|_error| "could not release recipe download URL".to_owned())?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_log_to_file(entries: &[LogEntry]) {
    let content = format_log(entries);
    if let Some(path) = rfd::FileDialog::new()
        .add_filter("Text", &["txt"])
        .set_file_name("frame_log.txt")
        .save_file()
    {
        std::fs::write(path, content).ok();
    }
}

#[cfg(target_arch = "wasm32")]
pub fn save_log_to_file(entries: &[LogEntry]) {
    use wasm_bindgen::JsCast as _;
    let content = format_log(entries);
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let array = js_sys::Array::new();
    array.push(&wasm_bindgen::JsValue::from_str(&content));
    let blob_opts = web_sys::BlobPropertyBag::new();
    blob_opts.set_type("text/plain");
    let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&array, &blob_opts) else {
        return;
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
        return;
    };
    let Ok(anchor) = document.create_element("a") else {
        return;
    };
    let anchor: web_sys::HtmlAnchorElement = anchor.unchecked_into();
    anchor.set_href(&url);
    anchor.set_download("frame_log.txt");
    anchor.click();
    web_sys::Url::revoke_object_url(&url).ok();
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn save_downloaded_file(file: &crate::backend::DownloadedFile) -> Result<(), String> {
    let Some(path) = rfd::FileDialog::new()
        .add_filter(&file.content_type, &["csv"])
        .set_file_name(&file.filename)
        .save_file()
    else {
        return Ok(());
    };
    std::fs::write(&path, &file.bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn save_downloaded_file(file: &crate::backend::DownloadedFile) -> Result<(), String> {
    use wasm_bindgen::JsCast as _;
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| "browser document is unavailable".to_owned())?;
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(file.bytes.as_slice()));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(&file.content_type);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)
        .map_err(|_error| "could not create CSV download".to_owned())?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|_error| "could not create CSV download URL".to_owned())?;
    let anchor: web_sys::HtmlAnchorElement = document
        .create_element("a")
        .map_err(|_error| "could not create download link".to_owned())?
        .unchecked_into();
    anchor.set_href(&url);
    anchor.set_download(&file.filename);
    let body = document
        .body()
        .ok_or_else(|| "browser document body is unavailable".to_owned())?;
    body.append_child(&anchor)
        .map_err(|_error| "could not attach download link".to_owned())?;
    anchor.click();
    anchor.remove();
    // Browsers may consume the blob after click returns. Keep it alive until
    // the download has had time to start, then release its backing memory.
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(10_000).await;
        let _released = web_sys::Url::revoke_object_url(&url);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_log_writes_hex_bytes() {
        let entries = [LogEntry {
            direction: LogDirection::In,
            label: "hello".to_owned(),
            timestamp: 65.0, // 1 minute, 5 seconds
            raw_bytes: vec![0xde, 0xad, 0xbe, 0xef],
        }];

        let out = format_log(&entries);

        assert!(out.contains("00:01:05"));
        assert!(out.contains("IN "));
        assert!(out.contains("hello"));
        assert!(out.contains("de ad be ef"));
    }

    #[test]
    fn format_log_empty_entries() {
        let entries: [LogEntry; 0] = [];
        let out = format_log(&entries);

        assert_eq!(out, "");
    }

    #[test]
    fn format_log_multiple_entries() {
        let entries = [
            LogEntry {
                direction: LogDirection::In,
                label: "first".to_owned(),
                timestamp: 10.0,
                raw_bytes: vec![0x01, 0x02],
            },
            LogEntry {
                direction: LogDirection::Out,
                label: "second".to_owned(),
                timestamp: 20.0,
                raw_bytes: vec![0x03, 0x04],
            },
        ];
        let out = format_log(&entries);

        assert!(out.contains("00:00:10"));
        assert!(out.contains("IN "));
        assert!(out.contains("first"));
        assert!(out.contains("01 02"));
        assert!(out.contains("00:00:20"));
        assert!(out.contains("OUT"));
        assert!(out.contains("second"));
        assert!(out.contains("03 04"));
    }
}
