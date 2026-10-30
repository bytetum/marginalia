//! Where the access token lives.
//!
//! The token is the key to the whole library, so it goes into the operating system's credential store whenever there
//! is one that works:
//!
//! | platform | store |
//! | --- | --- |
//! | macOS | Keychain (legacy file keychain, which also works for an unsigned app) |
//! | iOS | Keychain (protected data store, scoped to the app) |
//! | Windows | Credential Manager |
//! | Android | `SharedPreferences` encrypted with a key in the Android Keystore (`android-native-keyring-store`) |
//! | Linux | Secret Service (GNOME Keyring, KWallet) |
//!
//! If no store is available (a Linux box without a Secret Service, or an unexpected failure on a device), the token is
//! kept in a file in the app's private data directory, mode 0600 where the platform has modes, and the UI says so
//! (`storage()`), rather than failing or pretending. The server URL is not a secret and stays in the webview's own
//! storage.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

use keyring_core::{Entry, Error as KeyringError};

const SERVICE: &str = "com.bytetum.marginalia";
const USER: &str = "access-token";

/// Where the token currently is, as a phrase for the settings screen.
const KEYCHAIN: &str = "the system keychain";
const FILE: &str = "a private file in the app's data folder (no system keychain was available)";

pub struct Secrets {
    keyring_ready: bool,
    file: PathBuf,
    /// What the last operation used, for `storage()`.
    last: Mutex<&'static str>,
}

impl Secrets {
    pub fn new(data_dir: PathBuf) -> Self {
        let keyring_ready = match init_store() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("marginalia: no system credential store ({e}); the token will be kept in a private file");
                false
            }
        };
        Self {
            keyring_ready,
            file: data_dir.join("token"),
            last: Mutex::new(if keyring_ready { KEYCHAIN } else { FILE }),
        }
    }

    pub fn storage(&self) -> &'static str {
        self.last.lock().map(|g| *g).unwrap_or(FILE)
    }

    fn entry(&self) -> Option<Entry> {
        if !self.keyring_ready {
            return None;
        }
        Entry::new(SERVICE, USER).ok()
    }

    pub fn get(&self) -> Option<String> {
        if let Some(entry) = self.entry() {
            match entry.get_password() {
                Ok(token) if !token.is_empty() => return Some(token),
                Ok(_) | Err(KeyringError::NoEntry) => {}
                Err(e) => eprintln!("marginalia: reading the keychain failed: {e}"),
            }
        }
        let token = fs::read_to_string(&self.file).ok()?.trim().to_string();
        if token.is_empty() {
            return None;
        }
        self.set_last(FILE);
        Some(token)
    }

    /// Stores the token and returns where it went.
    pub fn set(&self, token: &str) -> Result<&'static str, String> {
        let token = token.trim();
        if token.is_empty() {
            return Err("the token is empty".into());
        }
        if let Some(entry) = self.entry() {
            match entry.set_password(token) {
                Ok(()) => {
                    // A token left in the fallback file by an earlier run must not outlive the keychain copy.
                    let _ = fs::remove_file(&self.file);
                    self.set_last(KEYCHAIN);
                    return Ok(KEYCHAIN);
                }
                Err(e) => eprintln!("marginalia: writing to the keychain failed ({e}); using the private file"),
            }
        }
        self.write_file(token).map_err(|e| format!("could not store the token: {e}"))?;
        self.set_last(FILE);
        Ok(FILE)
    }

    pub fn clear(&self) -> Result<(), String> {
        if let Some(entry) = self.entry() {
            match entry.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => {}
                Err(e) => eprintln!("marginalia: deleting from the keychain failed: {e}"),
            }
        }
        match fs::remove_file(&self.file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("could not remove the stored token: {e}")),
        }
    }

    fn set_last(&self, what: &'static str) {
        if let Ok(mut g) = self.last.lock() {
            *g = what;
        }
    }

    fn write_file(&self, token: &str) -> std::io::Result<()> {
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&self.file)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()
    }
}

#[cfg(target_os = "macos")]
fn init_store() -> Result<(), String> {
    let store = apple_native_keyring_store::keychain::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "ios")]
fn init_store() -> Result<(), String> {
    let store = apple_native_keyring_store::protected::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(windows)]
fn init_store() -> Result<(), String> {
    let store = windows_native_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "android")]
fn init_store() -> Result<(), String> {
    // Needs the NDK context that Tauri's Android runtime initialises; a missing one is an error here, not a crash.
    let store = std::panic::catch_unwind(android_native_keyring_store::Store::new)
        .map_err(|_| "the Android context is not initialised".to_string())?
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn init_store() -> Result<(), String> {
    let store = zbus_secret_service_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn init_store() -> Result<(), String> {
    Err("no credential store on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Secrets` that never touches a system credential store, so the file path is what is tested.
    fn file_only(dir: &std::path::Path) -> Secrets {
        Secrets { keyring_ready: false, file: dir.join("sub").join("token"), last: Mutex::new(FILE) }
    }

    #[test]
    fn file_fallback_round_trips_and_clears() {
        let dir = std::env::temp_dir().join(format!("marginalia-secret-test-{}", std::process::id()));
        let secrets = file_only(&dir);
        assert_eq!(secrets.get(), None);
        assert_eq!(secrets.set("  s3cret \n").unwrap(), FILE);
        assert_eq!(secrets.get().as_deref(), Some("s3cret"));
        assert_eq!(secrets.storage(), FILE);
        secrets.set("other").unwrap();
        assert_eq!(secrets.get().as_deref(), Some("other"));
        secrets.clear().unwrap();
        assert_eq!(secrets.get(), None);
        secrets.clear().unwrap(); // clearing twice is fine
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_tokens_are_refused() {
        let dir = std::env::temp_dir().join(format!("marginalia-secret-empty-{}", std::process::id()));
        let secrets = file_only(&dir);
        assert!(secrets.set("   ").is_err());
        assert_eq!(secrets.get(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn fallback_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("marginalia-secret-mode-{}", std::process::id()));
        let secrets = file_only(&dir);
        secrets.set("abc").unwrap();
        let mode = fs::metadata(&secrets.file).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "token file mode is {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }
}
