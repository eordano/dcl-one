use crate::core::prelude::{IntoString, PROCESS_MESSAGE_HOST_EMIT};
use bevy::log::{info, warn};
use cef::rc::{Rc, RcImpl};
use cef::{
    BeforeDownloadCallback, Browser, CefString, DownloadItem, DownloadItemCallback,
    ImplBeforeDownloadCallback, ImplBrowser, ImplDownloadHandler, ImplDownloadItem, ImplFrame,
    ImplListValue, ImplProcessMessage, ProcessId, WrapDownloadHandler, process_message_create, sys,
};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};

/// The id of the host event that tells the page how a download ended: `{ url, ok }`, with the
/// folder as `dir` when it was saved.
pub const DOWNLOAD_EVENT: &str = "download";

/// Only a `blob:` url of the page's own origin is saved, so an embedded frame can't write files.
///
/// ## Reference
///
/// - [`CefDownloadHandler Class Reference`](https://cef-builds.spotifycdn.com/docs/139.0/classCefDownloadHandler.html)
pub struct DownloadHandlerBuilder {
    object: *mut RcImpl<sys::_cef_download_handler_t, Self>,
}

impl DownloadHandlerBuilder {
    pub fn build() -> cef::DownloadHandler {
        cef::DownloadHandler::new(Self {
            object: core::ptr::null_mut(),
        })
    }
}

impl WrapDownloadHandler for DownloadHandlerBuilder {
    fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_download_handler_t, Self>) {
        self.object = object;
    }
}

impl Rc for DownloadHandlerBuilder {
    fn as_base(&self) -> &sys::cef_base_ref_counted_t {
        unsafe {
            let base = &*self.object;
            core::mem::transmute(&base.cef_object)
        }
    }
}

impl Clone for DownloadHandlerBuilder {
    fn clone(&self) -> Self {
        let object = unsafe {
            let rc_impl = &mut *self.object;
            rc_impl.interface.add_ref();
            rc_impl
        };
        Self { object }
    }
}

impl ImplDownloadHandler for DownloadHandlerBuilder {
    fn can_download(
        &self,
        browser: Option<&mut Browser>,
        url: Option<&CefString>,
        _request_method: Option<&CefString>,
    ) -> c_int {
        let url = url.map(|u| u.to_string()).unwrap_or_default();
        let page = browser
            .as_deref()
            .and_then(|b| b.main_frame())
            .map(|f| f.url().into_string())
            .unwrap_or_default();
        let allowed = is_own_blob(&url, &page);
        if !allowed {
            warn!("[cef] download refused: {url}");
            report(browser.as_deref(), &url, false);
        }
        allowed as c_int
    }

    fn on_before_download(
        &self,
        browser: Option<&mut Browser>,
        download_item: Option<&mut DownloadItem>,
        suggested_name: Option<&CefString>,
        callback: Option<&mut BeforeDownloadCallback>,
    ) -> c_int {
        let (Some(callback), Some(dir)) = (callback, downloads_dir()) else {
            warn!("[cef] download has no folder to save to");
            let url = download_item
                .map(|i| i.url().into_string())
                .unwrap_or_default();
            report(browser.as_deref(), &url, false);
            // Default handling, which for this embedding cancels the download.
            return 0;
        };
        let name = suggested_name.map(|n| n.to_string()).unwrap_or_default();
        let path = unused_path(&dir, &file_name(&name));
        callback.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
        1
    }

    fn on_download_updated(
        &self,
        browser: Option<&mut Browser>,
        download_item: Option<&mut DownloadItem>,
        _callback: Option<&mut DownloadItemCallback>,
    ) {
        let Some(item) = download_item else { return };
        let url = item.url().into_string();
        if item.is_complete() != 0 {
            info!("[cef] downloaded {}", item.full_path().into_string());
            report(browser.as_deref(), &url, true);
        } else if item.is_canceled() != 0 || item.is_interrupted() != 0 {
            warn!("[cef] download did not finish: {url}");
            report(browser.as_deref(), &url, false);
        }
    }

    #[inline]
    fn get_raw(&self) -> *mut sys::_cef_download_handler_t {
        self.object.cast()
    }
}

fn report(browser: Option<&Browser>, url: &str, ok: bool) {
    if let Some(frame) = browser.and_then(|b| b.main_frame())
        && let Some(mut message) = process_message_create(Some(&PROCESS_MESSAGE_HOST_EMIT.into()))
        && let Some(arguments) = message.argument_list()
    {
        let dir = ok.then(downloads_dir).flatten();
        let event = serde_json::json!({ "url": url, "ok": ok, "dir": dir }).to_string();
        arguments.set_string(0, Some(&DOWNLOAD_EVENT.into()));
        arguments.set_string(1, Some(&event.as_str().into()));
        frame.send_process_message(
            ProcessId::from(cef_dll_sys::cef_process_id_t::PID_RENDERER),
            Some(&mut message),
        );
    }
}

/// Where downloads are saved, created when missing.
pub fn downloads_dir() -> Option<PathBuf> {
    let dir = downloads_under(directories::UserDirs::new()?.home_dir());
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn downloads_under(home: &Path) -> PathBuf {
    home.join("Downloads")
}

fn origin(url: &str) -> Option<&str> {
    let after_scheme = url.find("://")? + 3;
    let end = url[after_scheme..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| after_scheme + i);
    Some(&url[..end])
}

fn is_own_blob(url: &str, page: &str) -> bool {
    let (Some(blob), Some(page)) = (url.strip_prefix("blob:"), origin(page)) else {
        return false;
    };
    origin(blob) == Some(page)
}

/// Only the last path segment, so the file can't land outside the folder.
fn file_name(suggested: &str) -> String {
    let name = suggested
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if name.is_empty() || name == "." || name == ".." {
        "download".to_owned()
    } else {
        name.to_owned()
    }
}

fn unused_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_blobs_of_the_page_origin() {
        let page = "cef://localhost/app.html?x=1";
        assert!(is_own_blob("blob:cef://localhost/1234-abcd", page));
        assert!(!is_own_blob("blob:https://evil.example/1234", page));
        assert!(!is_own_blob("https://cdn.example/photo.jpg", page));
        assert!(!is_own_blob("blob:cef://localhost/1234", ""));
    }

    #[test]
    fn saves_under_the_home_folder() {
        assert_eq!(
            downloads_under(Path::new("/home/ada")),
            Path::new("/home/ada/Downloads")
        );
    }

    #[test]
    fn keeps_the_name_inside_the_folder() {
        assert_eq!(file_name("photo.jpg"), "photo.jpg");
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("a\\b.jpg"), "b.jpg");
        assert_eq!(file_name(""), "download");
        assert_eq!(file_name(".."), "download");
    }

    #[test]
    fn does_not_overwrite_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("cef-dl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unused_path(&dir, "a.jpg"), dir.join("a.jpg"));
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        std::fs::write(dir.join("a (1).jpg"), b"x").unwrap();
        assert_eq!(unused_path(&dir, "a.jpg"), dir.join("a (2).jpg"));
        std::fs::write(dir.join("noext"), b"x").unwrap();
        assert_eq!(unused_path(&dir, "noext"), dir.join("noext (1)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
