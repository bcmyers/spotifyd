#![allow(dead_code)]

use std::borrow::Cow;
use std::cell::RefCell;
use std::env;
use std::ffi::CStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use path_absolutize::Absolutize;

const CONFIG_DIRECTORY_NAME: &str = "spotifyd";
const CONFIG_FILE_NAME: &str = "spotifyd.conf";

thread_local! {
    static BUF_HOSTNAME: RefCell<[libc::c_char; 255]> = RefCell::new([0; 255]);
    static BUF_USERNAME: RefCell<[libc::c_char; 255]> = RefCell::new([0; 255]);
}

pub(crate) fn absolutize_path<P>(path: P) -> io::Result<PathBuf>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let path_with_expanded_tilda: Cow<Path> = {
        let mut iter = path.iter();
        let first_component = iter
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Path may not be empty."))?;
        if first_component.as_bytes() == [b'~'] {
            let mut path = dirs::home_dir().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Unable to locate user's home directory.",
                )
            })?;
            while let Some(component) = iter.next() {
                path = path.join(component);
            }
            path.into()
        } else {
            path.into()
        }
    };
    path_with_expanded_tilda.absolutize()
}

pub(crate) fn get_config_path() -> Option<PathBuf> {
    if let Ok(dirs) = xdg::BaseDirectories::with_prefix(CONFIG_DIRECTORY_NAME) {
        if let Some(path) = dirs.find_config_file(CONFIG_FILE_NAME) {
            return Some(path);
        }
    }

    // On linux and macOS, look for config file in /etc ...
    #[cfg(not(any(
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
    )))]
    let etc_path = format!("/etc/{}", CONFIG_FILE_NAME);

    // On the BSDs, look for config file in /usr/local/etc ...
    #[cfg(any(
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
    ))]
    let etc_path = format!("/usr/local/etc/{}", CONFIG_FILE_NAME);

    if let Ok(meta) = fs::metadata(&etc_path) {
        if meta.is_file() {
            return Some(PathBuf::from(etc_path));
        }
    }

    None
}

pub(crate) fn get_hostname() -> Option<String> {
    BUF_HOSTNAME.with(|refcell| {
        let mut buf = refcell.borrow_mut();
        let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as _, buf.len() as _) };
        if ret != 0 {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
        let hostname = cstr.to_string_lossy().to_string();
        log::trace!("Found hostname {:?} using gethostname.", hostname);
        Some(hostname)
    })
}

pub(crate) fn get_shell() -> Option<String> {
    // First look for the user's preferred shell using the SHELL environment variable...
    if let Ok(shell) = env::var("SHELL") {
        log::trace!("Found shell {:?} using SHELL environment variable.", shell);
        return Some(shell);
    }

    // If the SHELL environment variable is not set and we're on linux or one of the BSDs,
    // try to obtain the default shell from `/etc/passwd`...
    #[cfg(not(target_os = "macos"))]
    {
        use std::fs::File;
        use std::io::BufRead;

        let username = get_username()?;

        let file = File::open("/etc/passwd").ok()?;
        let reader = io::BufReader::new(file);
        // Each line of `/etc/passwd` describes a single user and contains seven colon-separated fields:
        // "name:password:UID:GID:GECOS:directory:shell"
        for line in reader.lines() {
            let line = line.ok()?;
            let mut iter = line.split(':');
            if let Some(user) = iter.nth(0) {
                if user == username {
                    let shell = iter.nth(5)?;
                    log::trace!("Found shell {:?} using /etc/passwd.", shell);
                    return Some(shell.into());
                }
            }
        }
    }

    // If the SHELL environment variable is not set and on we're on macOS,
    // query the Directory Service command line utility (dscl) for the user's shell,
    // as macOS does not use the /etc/passwd file...
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;

        let username = get_username()?;
        let output = Command::new("dscl")
            .args(&[".", "-read", &format!("/Users/{}", username), "UserShell"])
            .output()
            .ok()?;
        if output.status.success() {
            let stdout = std::str::from_utf8(&output.stdout).ok()?;
            // The output of this dscl command should be:
            // "UserShell: /path/to/shell"
            if stdout.starts_with("UserShell: ") {
                let shell = stdout.split_whitespace().nth(1)?;
                log::trace!("Found shell {:?} using dscl command.", shell);
                return Some(shell.to_string());
            }
        }
    }
    None
}

fn get_username() -> Option<String> {
    BUF_USERNAME.with(|refcell| {
        let mut buf = refcell.borrow_mut();
        let ret = unsafe { getlogin_r(buf.as_mut_ptr() as _, buf.len() as _) };
        if ret != 0 {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
        let username = cstr.to_string_lossy().to_string();
        log::trace!("Found username: {:?} using getlogin_r", username);
        Some(username)
    })
}

extern "C" {
    fn getlogin_r(buf: *mut libc::c_char, size: libc::size_t) -> libc::c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_absolutize_path() -> io::Result<()> {
        let home_dir = dirs::home_dir().expect("Unable to locate user's home directory.");

        // Empty path returns an error.
        let actual = absolutize_path("");
        assert!(actual.is_err());

        // "~" alone expands to home directory.
        let actual = absolutize_path("~")?;
        let expected = &home_dir;
        assert_eq!(&actual, expected);

        // "~/foo" expands to "<home-directory>/foo".
        let actual = absolutize_path("~/foo")?;
        let expected = home_dir.join("foo");
        assert_eq!(actual, expected);

        // "~/foo/.." expands to home directory.
        let actual = absolutize_path("~/foo/..")?;
        let expected = &home_dir;
        assert_eq!(&actual, expected);

        // "/foo/~" remains unchanged.
        let actual = absolutize_path("/foo/~")?;
        let expected = Path::new("/foo/~");
        assert_eq!(&actual, expected);

        // "/foo/~/foo" remains unchanged.
        let actual = absolutize_path("/foo/~/foo")?;
        let expected = Path::new("/foo/~/foo");
        assert_eq!(&actual, expected);

        // "/~foo/foo" remains unchanged.
        let actual = absolutize_path("/~foo/foo")?;
        let expected = Path::new("/~foo/foo");
        assert_eq!(&actual, expected);

        Ok(())
    }

    #[test]
    fn test_get_shell() {
        env::set_var("RUST_LOG", "spotifyd=trace");
        env_logger::init();
        let _ = get_hostname().unwrap();
        let _ = get_shell().unwrap();
        if env::var("SHELL").is_ok() {
            env::remove_var("SHELL");
            let _ = get_shell().unwrap();
        }
    }

}
