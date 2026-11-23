// SPDX-License-Identifier: MPL-2.0
//! HTTP fetch of package files. Files are requested together and the browser's own cache applies.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::Response;

use super::js::describe;
use crate::WebError;

fn window() -> Result<web_sys::Window, WebError> {
    web_sys::window().ok_or_else(|| WebError::new("backend_unavailable", "no window"))
}

/// One file's bytes. `code` is the error code to report on failure.
pub async fn fetch_bytes(url: &str, code: &'static str) -> Result<Vec<u8>, WebError> {
    let fail = |what: String| WebError::new(code, format!("{url}: {what}"));
    let response = JsFuture::from(window()?.fetch_with_str(url))
        .await
        .map_err(|e| fail(describe(&e)))?;
    let response: Response = response.dyn_into().map_err(|_| fail("not a response".into()))?;
    if !response.ok() {
        return Err(fail(format!("HTTP {}", response.status())));
    }
    let buffer = response.array_buffer().map_err(|e| fail(describe(&e)))?;
    let buffer = JsFuture::from(buffer).await.map_err(|e| fail(describe(&e)))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// Several files, fetched in parallel, in the order of `urls`.
pub async fn fetch_all(urls: &[String], code: &'static str) -> Result<Vec<Vec<u8>>, WebError> {
    let window = window()?;
    let promises = js_sys::Array::new();
    for url in urls {
        promises.push(&JsValue::from(window.fetch_with_str(url)));
    }
    let responses = JsFuture::from(js_sys::Promise::all(&promises))
        .await
        .map_err(|e| WebError::new(code, describe(&e)))?;
    let responses: js_sys::Array = responses.unchecked_into();
    let buffers = js_sys::Array::new();
    for (i, url) in urls.iter().enumerate() {
        let response: Response = responses
            .get(i as u32)
            .dyn_into()
            .map_err(|_| WebError::new(code, format!("{url}: not a response")))?;
        if !response.ok() {
            return Err(WebError::new(code, format!("{url}: HTTP {}", response.status())));
        }
        let buffer = response
            .array_buffer()
            .map_err(|e| WebError::new(code, describe(&e)))?;
        buffers.push(&buffer);
    }
    let buffers = JsFuture::from(js_sys::Promise::all(&buffers))
        .await
        .map_err(|e| WebError::new(code, describe(&e)))?;
    let buffers: js_sys::Array = buffers.unchecked_into();
    Ok(buffers
        .iter()
        .map(|b| js_sys::Uint8Array::new(&b).to_vec())
        .collect())
}
