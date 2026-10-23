//! Put the install folder, which holds the `diskghost` CLI next to the app, on
//! the user's PATH.
//!
//! The Windows setup runs the app once with `--add-to-path` after installing
//! and with `--remove-from-path` before uninstalling. The entry is written
//! straight to `HKCU\Environment` with the value's own type kept
//! (`REG_EXPAND_SZ` entries like `%USERPROFILE%\bin` stay unexpanded) and no
//! length limit; only the entry for this folder is ever added or removed.

/// True if a PATH entry names `dir` (case-insensitive, quotes and trailing
/// slashes ignored).
#[cfg(any(windows, test))]
fn same_dir(entry: &str, dir: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_matches('"')
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    let entry = norm(entry);
    !entry.is_empty() && entry == norm(dir)
}

/// `path` with `dir` appended, or `None` when it already lists `dir`.
#[cfg(any(windows, test))]
fn with_dir(path: &str, dir: &str) -> Option<String> {
    if path.split(';').any(|e| same_dir(e, dir)) {
        return None;
    }
    let rest = path.trim_end_matches(';');
    Some(if rest.is_empty() {
        dir.to_string()
    } else {
        format!("{rest};{dir}")
    })
}

/// `path` without the entries naming `dir`, or `None` when it lists none.
#[cfg(any(windows, test))]
fn without_dir(path: &str, dir: &str) -> Option<String> {
    if !path.split(';').any(|e| same_dir(e, dir)) {
        return None;
    }
    Some(
        path.split(';')
            .filter(|e| !same_dir(e, dir))
            .collect::<Vec<_>>()
            .join(";"),
    )
}

/// Add (`add`) or remove the folder of this executable to/from the user's
/// PATH. Adding is skipped when the CLI is not next to the app (a dev build).
#[cfg(windows)]
pub fn update(add: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("the app has no folder")?;
    if add && !dir.join("diskghost.exe").is_file() {
        return Ok(());
    }
    let dir = dir
        .to_str()
        .ok_or("the app's folder is not valid Unicode")?;
    let key = reg::Key::environment()?;
    let (old, kind) = key.path()?;
    let new = if add {
        with_dir(&old, dir)
    } else {
        without_dir(&old, dir)
    };
    let Some(new) = new else {
        return Ok(()); // already as wanted
    };
    if new.trim_matches(';').is_empty() {
        key.delete_path()?;
    } else {
        key.set_path(&new, kind)?;
    }
    reg::announce();
    Ok(())
}

#[cfg(not(windows))]
pub fn update(_add: bool) -> Result<(), String> {
    Err("only the Windows setup puts the CLI on the PATH".into())
}

/// The few registry calls this needs (advapi32 / user32).
#[cfg(windows)]
mod reg {
    use std::ffi::c_void;

    type Hkey = *mut c_void;

    // Sign-extended like the SDK's `(HKEY)(ULONG_PTR)(LONG)0x80000001`.
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001_u32 as i32 as isize as Hkey;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_MORE_DATA: i32 = 234;
    const HWND_BROADCAST: *mut c_void = 0xffff as *mut c_void;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(key: Hkey, sub: *const u16, opts: u32, sam: u32, out: *mut Hkey) -> i32;
        fn RegQueryValueExW(
            key: Hkey,
            name: *const u16,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            len: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            key: Hkey,
            name: *const u16,
            reserved: u32,
            kind: u32,
            data: *const u8,
            len: u32,
        ) -> i32;
        fn RegDeleteValueW(key: Hkey, name: *const u16) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            hwnd: *mut c_void,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(code: i32, what: &str) -> Result<(), String> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "{what}: {}",
                std::io::Error::from_raw_os_error(code)
            ))
        }
    }

    /// `HKCU\Environment`, open for reading and writing.
    pub struct Key(Hkey);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    impl Key {
        pub fn environment() -> Result<Key, String> {
            let mut key: Hkey = std::ptr::null_mut();
            let sub = wide("Environment");
            let code = unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    &mut key,
                )
            };
            check(code, "cannot open HKCU\\Environment")?;
            Ok(Key(key))
        }

        /// The raw (unexpanded) `Path` value and its type; empty and
        /// `REG_EXPAND_SZ` when there is none.
        pub fn path(&self) -> Result<(String, u32), String> {
            let name = wide("Path");
            let mut buf: Vec<u16> = vec![0; 1024];
            loop {
                let mut kind = 0u32;
                let mut len = (buf.len() * 2) as u32;
                let code = unsafe {
                    RegQueryValueExW(
                        self.0,
                        name.as_ptr(),
                        std::ptr::null_mut(),
                        &mut kind,
                        buf.as_mut_ptr().cast(),
                        &mut len,
                    )
                };
                match code {
                    ERROR_FILE_NOT_FOUND => return Ok((String::new(), REG_EXPAND_SZ)),
                    ERROR_MORE_DATA => buf.resize((len as usize).div_ceil(2) + 1, 0),
                    _ => {
                        check(code, "cannot read the user PATH")?;
                        if kind != REG_SZ && kind != REG_EXPAND_SZ {
                            return Err(format!("the user PATH has registry type {kind}"));
                        }
                        buf.truncate((len as usize) / 2);
                        while buf.last() == Some(&0) {
                            buf.pop();
                        }
                        return Ok((String::from_utf16_lossy(&buf), kind));
                    }
                }
            }
        }

        pub fn set_path(&self, value: &str, kind: u32) -> Result<(), String> {
            let name = wide("Path");
            let data = wide(value);
            let code = unsafe {
                RegSetValueExW(
                    self.0,
                    name.as_ptr(),
                    0,
                    kind,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            check(code, "cannot write the user PATH")
        }

        pub fn delete_path(&self) -> Result<(), String> {
            let name = wide("Path");
            let code = unsafe { RegDeleteValueW(self.0, name.as_ptr()) };
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            check(code, "cannot remove the user PATH")
        }
    }

    /// Tell running programs (Explorer, so new terminals) that the
    /// environment changed.
    pub fn announce() {
        let what = wide("Environment");
        let mut result = 0usize;
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                what.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = r"C:\Users\me\AppData\Local\Diskghost";

    #[test]
    fn adds_once() {
        assert_eq!(with_dir("", DIR).as_deref(), Some(DIR));
        assert_eq!(
            with_dir(r"%USERPROFILE%\bin;C:\tools", DIR).unwrap(),
            format!(r"%USERPROFILE%\bin;C:\tools;{DIR}")
        );
        // A trailing separator does not leave an empty entry behind.
        assert_eq!(
            with_dir(r"C:\tools;", DIR).unwrap(),
            format!(r"C:\tools;{DIR}")
        );
        // Already listed, in any spelling: nothing to do.
        let listed = format!(r"C:\tools;{DIR}");
        assert_eq!(with_dir(&listed, DIR), None);
        assert_eq!(with_dir(r"c:\users\ME\appdata\local\diskghost\", DIR), None);
        assert_eq!(with_dir(&format!("\"{DIR}\""), DIR), None);
    }

    #[test]
    fn removes_only_this_folder() {
        assert_eq!(without_dir(r"C:\tools", DIR), None);
        assert_eq!(without_dir("", DIR), None);
        assert_eq!(
            without_dir(&format!(r"%USERPROFILE%\bin;{DIR};C:\tools"), DIR).unwrap(),
            r"%USERPROFILE%\bin;C:\tools"
        );
        // Every copy goes, in any spelling; other entries stay as they were.
        assert_eq!(
            without_dir(&format!(r"{DIR};C:\a;;{}\", DIR.to_uppercase()), DIR).unwrap(),
            r"C:\a;"
        );
        assert_eq!(without_dir(DIR, DIR).unwrap(), "");
        // A folder that merely starts the same is someone else's.
        assert_eq!(without_dir(&format!(r"{DIR}2"), DIR), None);
    }

    #[test]
    fn add_then_remove_restores() {
        for path in ["", r"C:\a", r"C:\a;%X%\b"] {
            let added = with_dir(path, DIR).unwrap();
            assert_eq!(without_dir(&added, DIR).unwrap(), path);
        }
    }
}
