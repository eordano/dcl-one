use std::{future::Future, pin::Pin, sync::OnceLock};

use wasm_bindgen_futures::JsFuture;
use web_sys::window;

use async_trait::async_trait;

use crate::common::{ClipboardProvider, Result};

pub type ClipboardFuture<T> = Pin<Box<dyn Future<Output = std::result::Result<T, String>>>>;

/// Clipboard access supplied by the embedder, for contexts without a `window` (e.g. a web
/// worker, whose `navigator` has no clipboard). When set, [`WasmClipboardContext`] calls these
/// instead of `navigator.clipboard`.
pub struct ExternalClipboard {
    pub get: fn() -> ClipboardFuture<String>,
    pub set: fn(String) -> ClipboardFuture<()>,
}

static EXTERNAL: OnceLock<ExternalClipboard> = OnceLock::new();

/// Routes clipboard access through `clipboard` from now on. Only the first call takes effect;
/// returns whether this one did.
pub fn set_external_clipboard(clipboard: ExternalClipboard) -> bool {
    EXTERNAL.set(clipboard).is_ok()
}

pub struct WasmClipboardContext;

impl WasmClipboardContext {
    pub fn new() -> Result<Self> {
        Ok(WasmClipboardContext)
    }
}

#[async_trait(?Send)]
impl ClipboardProvider for WasmClipboardContext {
    async fn get_contents(&mut self) -> Result<String> {
        if let Some(external) = EXTERNAL.get() {
            return Ok((external.get)().await?);
        }
        let Some(window) = window() else {
            return Err("Should have a window in this context".into());
        };
        let navigator = window.navigator();
        let result = JsFuture::from(navigator.clipboard().read_text())
            .await
            .map_err(|jse| format!("{jse:?}"))?;
        Ok(result.as_string().unwrap_or_default())
    }

    async fn set_contents(&mut self, data: String) -> Result<()> {
        if let Some(external) = EXTERNAL.get() {
            return Ok((external.set)(data).await?);
        }
        let Some(window) = window() else {
            return Err("Should have a window in this context".into());
        };
        let navigator = window.navigator();
        let promise = navigator.clipboard().write_text(&data);
        JsFuture::from(promise).await.map(|_| ()).map_err(|jse| format!("{jse:?}").into())
    }
}
