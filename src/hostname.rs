// Short host name for the prompt.
pub fn gethostname() -> Option<String> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        unsafe extern "C" {
            fn gethostname(buf: *mut libc::c_char, size: libc::size_t) -> libc::c_int;
        }
        let mut buf = [0u8; 256];
        unsafe {
            let rc = gethostname(buf.as_mut_ptr() as *mut _, buf.len());
            if rc == 0 {
                let cstr = CStr::from_bytes_until_nul(&buf).ok()?;
                return Some(cstr.to_string_lossy().into_owned());
            }
        }
        None
    }

    #[cfg(not(unix))]
    {
        None
    }
}
