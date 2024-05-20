//! wchar
//!
//! Some crap code for dealing with Os u16 chars
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
#[allow(unused_macros)]

/// Convert a u16 array into an OsString.
///
/// Safety: The u16 array must be null terminated
pub unsafe fn from_wide(ptr: *const u16) -> OsString {
    let mut seek = ptr;
    loop {
        if *seek == 0 {
            break;
        } else {
            seek = seek.add(1);
        }
    }
    let len = (seek as usize - ptr as usize) / std::mem::size_of::<u16>();
    OsString::from_wide(std::slice::from_raw_parts(ptr, len))
}

#[macro_export]
macro_rules! get_window_text {
    ($hwnd:expr, $max:expr) => {{
        let buff: [u16; $max + 1] = [0; $max + 1];
        let result = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW(
                $hwnd,
                buff.as_ptr() as _,
                buff.len() as _,
            )
        };
        match result as _ {
            0..=$max => Ok(unsafe { from_wide(buff.as_ptr()) }),
            _ => Err(io::Error::last_os_error()),
        }
    }};
}

pub fn to_wide<O>(s: O) -> Vec<u16>
where
    O: Into<OsString>,
{
    use std::os::windows::prelude::*;
    s.into().encode_wide().chain(Some(0).into_iter()).collect()
}
