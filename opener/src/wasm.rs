use crate::OpenError;
use std::ffi::OsStr;
use std::io;

pub fn open(path: &OsStr) -> Result<(), OpenError> {
    let url = path.to_string_lossy();
    let window = web_sys::window().ok_or(OpenError::Io(io::Error::other("window not found")))?;
    window
        .open_with_url_and_target(&url, "_blank")
        .map(|_| ())
        .map_err(|e| OpenError::Io(io::Error::other(format!("{e:?}"))))
}
