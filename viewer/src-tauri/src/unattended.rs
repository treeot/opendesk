//! Unattended access secrets live in the OS keyring (Keychain, Credential
//! Manager, Secret Service); the config only records that one was saved. The
//! keyring can be missing (Linux without a Secret Service), locked, or wiped,
//! so every read and write here turns a failure into a message the user can
//! act on, and unattended access only counts as ready after a successful read.

use crate::config::Config;
use keyring::{Entry, Error};
#[cfg(windows)]
#[allow(unused_imports)]
use std::{collections::HashMap, path::{Path, PathBuf}};

pub const SERVICE: &str = "opendesk";
pub const USER: &str = "unattended";
pub const TOTP_USER: &str = "unattended-totp";

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStoreKind {
    UserKeyring,
    MachineProtected,
}

/// Select the scope required by the process that will read the secret.
/// A signed-in user keeps the existing keyring behavior; only the helper
/// needs the machine-protected store while no interactive user exists.
#[allow(dead_code)]
pub fn secret_store_kind(helper_active: bool, user_signed_in: bool) -> SecretStoreKind {
    if helper_active && !user_signed_in { SecretStoreKind::MachineProtected } else { SecretStoreKind::UserKeyring }
}

const PASSWORD: &str = "the unattended access password";
const TOTP: &str = "the 2FA secret";

/// Where secrets are kept. The real one is [`Keyring`]; tests use a fake.
pub trait SecretStore {
    fn get(&self, user: &str) -> keyring::Result<String>;
    fn set(&self, user: &str, secret: &str) -> keyring::Result<()>;
    fn delete(&self, user: &str) -> keyring::Result<()>;
}

/// The platform keyring.
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, user: &str) -> keyring::Result<String> {
        Entry::new(SERVICE, user)?.get_password()
    }
    fn set(&self, user: &str, secret: &str) -> keyring::Result<()> {
        Entry::new(SERVICE, user)?.set_password(secret)
    }
    fn delete(&self, user: &str) -> keyring::Result<()> {
        Entry::new(SERVICE, user)?.delete_credential()
    }
}

#[cfg(windows)]
pub struct MachineStore;

#[cfg(windows)]
fn machine_store_path() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("OpenDesk").join("secrets.json")
}

#[cfg(windows)]
fn machine_error(error: impl std::fmt::Display) -> Error {
    Error::PlatformFailure(Box::new(std::io::Error::other(error.to_string())))
}

/// DPAPI with the machine scope. The secrets are never written in the clear:
/// only code running on this machine (the SYSTEM service and Administrators,
/// to whom the file is also locked down) can decrypt them, and a stolen disk
/// cannot. The machine scope lets the signed-in user encrypt a secret that the
/// SYSTEM service later reads.
#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPT_INTEGER_BLOB};

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        unsafe {
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as *mut core::ffi::c_void);
            bytes
        }
    }

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        let input = blob(plain);
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe { CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_LOCAL_MACHINE, &mut out) };
        (ok != 0).then(|| take(out))
    }

    pub fn unprotect(sealed: &[u8]) -> Option<Vec<u8>> {
        let input = blob(sealed);
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe { CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_LOCAL_MACHINE, &mut out) };
        (ok != 0).then(|| take(out))
    }
}

/// Write a secrets file so only SYSTEM and Administrators can read it. The
/// directory's inheritable permissions are reset *before* the file is created,
/// so the file never exists with the broad ProgramData defaults, and the file
/// itself is locked down too as a belt-and-braces second step.
#[cfg(windows)]
fn write_locked(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("machine secret directory is missing")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let lock = |target: &str| {
        std::process::Command::new("icacls")
            .args([target, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
            .status()
            .map_err(|error| error.to_string())
            .and_then(|status| status.success().then_some(()).ok_or_else(|| "could not protect the machine secret store".to_owned()))
    };
    lock(&dir.to_string_lossy())?;
    std::fs::write(path, bytes).map_err(|error| error.to_string())?;
    lock(&path.to_string_lossy())
}

/// Read and decrypt the machine secrets map. Missing file, failed decryption
/// or bad JSON all read as "no secrets" so a tampered or stale file can never
/// crash the service; it just falls back to needing approval.
#[cfg(windows)]
fn read_machine_secrets() -> Result<HashMap<String, String>, Error> {
    let sealed = match std::fs::read(machine_store_path()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(Error::NoEntry),
        Err(error) => return Err(machine_error(error)),
    };
    let plain = dpapi::unprotect(&sealed).ok_or(Error::NoEntry)?;
    Ok(serde_json::from_slice(&plain).unwrap_or_default())
}

#[cfg(windows)]
fn write_machine_secrets(secrets: &HashMap<String, String>) -> keyring::Result<()> {
    let plain = serde_json::to_vec(secrets).map_err(machine_error)?;
    let sealed = dpapi::protect(&plain).ok_or_else(|| machine_error("DPAPI could not encrypt the machine secret store"))?;
    write_locked(&machine_store_path(), &sealed).map_err(machine_error)
}

#[cfg(windows)]
impl SecretStore for MachineStore {
    fn get(&self, user: &str) -> keyring::Result<String> {
        read_machine_secrets()?.get(user).cloned().ok_or(Error::NoEntry)
    }
    fn set(&self, user: &str, secret: &str) -> keyring::Result<()> {
        let mut secrets = read_machine_secrets().unwrap_or_default();
        secrets.insert(user.to_owned(), secret.to_owned());
        write_machine_secrets(&secrets)
    }
    fn delete(&self, user: &str) -> keyring::Result<()> {
        let mut secrets = read_machine_secrets()?;
        if secrets.remove(user).is_none() { return Err(Error::NoEntry); }
        write_machine_secrets(&secrets)
    }
}

#[cfg(windows)]
pub fn stage_machine_store(data_dir: &Path) -> Result<(), String> {
    let password = match read_password(&Keyring) { Lookup::Found(value) => value, Lookup::Missing => return Err("save unattended access before enabling the Windows sign-in service".into()), Lookup::Unavailable(error) => return Err(error) };
    let mut secrets = HashMap::from([(USER.to_owned(), password)]);
    if let Lookup::Found(secret) = read_totp_secret(&Keyring) { secrets.insert(TOTP_USER.to_owned(), secret); }
    // Encrypt before the secrets ever touch the disk, and lock the staged file
    // down too: it sits in ProgramData until the service promotes it.
    let plain = serde_json::to_vec(&secrets).map_err(|error| error.to_string())?;
    let sealed = dpapi::protect(&plain).ok_or("could not encrypt the staged secrets")?;
    write_locked(&data_dir.join("service-secrets.json"), &sealed)
}

/// Remove the machine secret store. Called when the sign-in service is
/// uninstalled so the unattended secret does not linger on disk.
#[cfg(windows)]
pub fn clear_machine_store() {
    let _ = std::fs::remove_file(machine_store_path());
}

#[cfg(windows)]
pub fn promote_staged_machine_store(data_dir: &Path) -> Result<(), String> {
    let staged = data_dir.join("service-secrets.json");
    // The staged file is already a DPAPI blob; move it to the machine store
    // unchanged, locked down so only SYSTEM and Administrators can read it.
    let sealed = std::fs::read(&staged).map_err(|error| error.to_string())?;
    write_locked(&machine_store_path(), &sealed)?;
    let _ = std::fs::remove_file(staged);
    Ok(())
}

#[cfg(windows)]
pub fn check_runtime_config(config: &Config) -> Check {
    check(&MachineStore, config.unattended_enabled, config.unattended_password_set, config.unattended_twofa_enabled)
}

/// What to do when the keyring can't be used on this platform.
fn keyring_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Unlock your login keychain in Keychain Access, then try again."
    } else if cfg!(windows) {
        "Make sure Windows Credential Manager is available, then try again."
    } else {
        "Install and unlock a keyring such as GNOME Keyring or KWallet, then try again."
    }
}

#[derive(Clone, Copy)]
enum Action {
    Save,
    Read,
    Remove,
}

fn keyring_failure(action: Action, what: &str, error: &Error) -> String {
    let (verb, preposition) = match action {
        Action::Save => ("save", "to"),
        Action::Read => ("read", "from"),
        Action::Remove => ("remove", "from"),
    };
    log::warn!("could not {verb} {what} {preposition} the keyring: {error}");
    let reason = if matches!(error, Error::NoDefaultStore) {
        format!("OpenDesk couldn't find a system keyring to {verb} {what}.")
    } else {
        format!("OpenDesk couldn't {verb} {what} {preposition} the system keyring.")
    };
    match action {
        Action::Read => format!("{reason} Until it can, connections need your approval. {}", keyring_hint()),
        Action::Save | Action::Remove => format!("{reason} {}", keyring_hint()),
    }
}

/// The result of reading one secret.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(String),
    /// Never saved, or removed from the keyring outside OpenDesk.
    Missing,
    /// The keyring itself failed; holds the message for the user.
    Unavailable(String),
}

fn read(store: &impl SecretStore, user: &str, what: &str) -> Lookup {
    match store.get(user) {
        Ok(secret) => Lookup::Found(secret),
        Err(Error::NoEntry) => Lookup::Missing,
        Err(error) => Lookup::Unavailable(keyring_failure(Action::Read, what, &error)),
    }
}

pub fn read_password(store: &impl SecretStore) -> Lookup {
    read(store, USER, PASSWORD)
}

pub fn read_totp_secret(store: &impl SecretStore) -> Lookup {
    read(store, TOTP_USER, TOTP)
}

pub fn save_password(store: &impl SecretStore, password: &str) -> Result<(), String> {
    store.set(USER, password).map_err(|error| keyring_failure(Action::Save, PASSWORD, &error))
}

pub fn save_totp_secret(store: &impl SecretStore, secret: &str) -> Result<(), String> {
    store.set(TOTP_USER, secret).map_err(|error| keyring_failure(Action::Save, TOTP, &error))
}

/// A secret that is already gone counts as removed, so 2FA can always be
/// turned off after the keyring lost it.
pub fn delete_totp_secret(store: &impl SecretStore) -> Result<(), String> {
    match store.delete(TOTP_USER) {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_failure(Action::Remove, TOTP, &error)),
    }
}

/// Saves into the platform keyring; used by the legacy config migration.
pub fn set_password(password: &str) -> Result<(), String> {
    save_password(&Keyring, password)
}

const SAVED: &str = "the saved device password";

/// Keyring entry for a saved remote-device password, namespaced so it never
/// collides with the unattended-access secrets.
fn contact_user(id: &str) -> String {
    format!("contact:{id}")
}

pub fn read_contact_password(store: &impl SecretStore, id: &str) -> Lookup {
    read(store, &contact_user(id), SAVED)
}

/// True when a password is saved for `id` and the keyring could be read.
pub fn has_contact_password(store: &impl SecretStore, id: &str) -> bool {
    matches!(read_contact_password(store, id), Lookup::Found(_))
}

pub fn save_contact_password(store: &impl SecretStore, id: &str, password: &str) -> Result<(), String> {
    store.set(&contact_user(id), password).map_err(|error| keyring_failure(Action::Save, SAVED, &error))
}

/// A password that is already gone counts as removed, so forgetting one is
/// idempotent even after the keyring lost it.
pub fn delete_contact_password(store: &impl SecretStore, id: &str) -> Result<(), String> {
    match store.delete(&contact_user(id)) {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_failure(Action::Remove, SAVED, &error)),
    }
}

const PASSWORD_MISSING: &str = "The unattended access password is missing from the system keyring, so connections need your approval. Enter the password again and save.";
const TOTP_MISSING: &str = "The 2FA secret is missing from the system keyring, so connections need your approval. Turn off \"Require 2FA code\" and set it up again.";

/// Unattended access as it would work right now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Check {
    /// A password is saved. While unattended access is off, or the keyring
    /// can't be read, this is the config marker; it only picks the password
    /// field's placeholder.
    pub has_password: bool,
    /// Set only when unattended access is on and every secret it needs was
    /// read. `None` means connections fall back to manual approval.
    pub password: Option<String>,
    pub totp_secret: Option<String>,
    /// Why unattended access is on but not working, in words for the user.
    pub problem: Option<String>,
}

impl Check {
    pub fn ready(&self) -> bool {
        self.password.is_some()
    }
}

/// Read what unattended access needs. `password_marker` is the config's
/// record that a password was saved; `twofa` whether 2FA is turned on.
pub fn check(store: &impl SecretStore, enabled: bool, password_marker: bool, twofa: bool) -> Check {
    // Off means no keyring access at all, so people who never use unattended
    // access never hit a missing or locked keyring. Turning it on re-checks.
    if !enabled {
        return Check { has_password: password_marker, ..Check::default() };
    }
    let lookup = read_password(store);
    let has_password = match &lookup {
        Lookup::Found(_) => true,
        Lookup::Missing => false,
        Lookup::Unavailable(_) => password_marker,
    };
    let not_ready = |problem: String| Check { has_password, problem: Some(problem), ..Check::default() };
    let password = match lookup {
        Lookup::Found(password) => password,
        Lookup::Missing => return not_ready(PASSWORD_MISSING.into()),
        Lookup::Unavailable(message) => return not_ready(message),
    };
    if !twofa {
        return Check { has_password, password: Some(password), ..Check::default() };
    }
    match read_totp_secret(store) {
        Lookup::Found(secret) => Check { has_password, password: Some(password), totp_secret: Some(secret), problem: None },
        Lookup::Missing => not_ready(TOTP_MISSING.into()),
        Lookup::Unavailable(message) => not_ready(message),
    }
}

/// [`check`] against the platform keyring with the saved settings.
pub fn check_config(config: &Config) -> Check {
    check(&Keyring, config.unattended_enabled, config.unattended_password_set, config.unattended_twofa_enabled)
}

/// Save the unattended access settings. A new password is written first; the
/// config marker then follows what the keyring actually holds. Enabling needs
/// a password that can be read back, so a broken keyring fails here instead
/// of later, when someone tries to connect.
pub fn set_access(store: &impl SecretStore, config: &mut Config, enabled: bool, password: Option<&str>) -> Result<(), String> {
    if let Some(password) = password {
        save_password(store, password)?;
    }
    match read_password(store) {
        Lookup::Found(_) => config.unattended_password_set = true,
        Lookup::Missing if enabled => return Err("enter an unattended access password before enabling it".into()),
        Lookup::Missing => config.unattended_password_set = false,
        Lookup::Unavailable(message) if enabled => return Err(message),
        // Turning it off still works; the marker is left as it was.
        Lookup::Unavailable(_) => {}
    }
    config.unattended_enabled = enabled;
    Ok(())
}

/// Whether 2FA is on with its secret in place, so enrolling again is refused.
pub fn twofa_active(store: &impl SecretStore, config: &Config) -> bool {
    config.unattended_twofa_enabled && matches!(read_totp_secret(store), Lookup::Found(_))
}

/// Turn 2FA off. The secret is removed when 2FA was on or one is left over;
/// one the keyring already lost is fine.
pub fn disable_twofa(store: &impl SecretStore, config: &mut Config) -> Result<(), String> {
    if config.unattended_twofa_enabled || matches!(read_totp_secret(store), Lookup::Found(_)) {
        delete_totp_secret(store)?;
    }
    config.unattended_twofa_enabled = false;
    Ok(())
}

/// Store a confirmed 2FA secret and turn 2FA on.
pub fn enable_twofa(store: &impl SecretStore, config: &mut Config, secret: &str) -> Result<(), String> {
    save_totp_secret(store, secret)?;
    config.unattended_twofa_enabled = true;
    Ok(())
}

/// A 2FA secret generated by `set_unattended_2fa` but not yet confirmed with
/// a code. It is never written to the keychain until confirmation, so an
/// abandoned enrollment cannot leave 2FA half-enabled.
#[derive(Default)]
pub struct PendingTotp(pub std::sync::Mutex<Option<String>>);

impl PendingTotp {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `otpauth://` URI with the account labelled `OpenDesk:<device id>`.
pub fn otpauth_uri(device_id: &str, secret: &str) -> String {
    format!("otpauth://totp/OpenDesk:{device_id}?secret={secret}&issuer=OpenDesk&algorithm=SHA1&digits=6&period=30")
}

/// QR code modules for `data` as (width, row-major dark flags).
pub fn qr_modules(data: &str) -> Result<(usize, Vec<bool>), String> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|error| error.to_string())?;
    let modules = code.to_colors().into_iter().map(|color| color == qrcode::Color::Dark).collect();
    Ok((code.width(), modules))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    /// In-memory keyring. Users listed in `broken` fail every call with the
    /// error `fail` builds, the way a locked or missing keyring does.
    #[derive(Default)]
    struct FakeStore {
        secrets: RefCell<HashMap<String, String>>,
        broken: Vec<&'static str>,
        fail: Option<fn() -> Error>,
    }

    impl FakeStore {
        fn with(secrets: &[(&str, &str)]) -> Self {
            let secrets = secrets.iter().map(|(user, secret)| (user.to_string(), secret.to_string())).collect();
            Self { secrets: RefCell::new(secrets), ..Self::default() }
        }
        fn broken(users: &[&'static str], fail: fn() -> Error) -> Self {
            Self { broken: users.to_vec(), fail: Some(fail), ..Self::default() }
        }
        fn failure(&self, user: &str) -> keyring::Result<()> {
            match self.fail {
                Some(fail) if self.broken.contains(&user) => Err(fail()),
                _ => Ok(()),
            }
        }
    }

    impl SecretStore for FakeStore {
        fn get(&self, user: &str) -> keyring::Result<String> {
            self.failure(user)?;
            self.secrets.borrow().get(user).cloned().ok_or(Error::NoEntry)
        }
        fn set(&self, user: &str, secret: &str) -> keyring::Result<()> {
            self.failure(user)?;
            self.secrets.borrow_mut().insert(user.into(), secret.into());
            Ok(())
        }
        fn delete(&self, user: &str) -> keyring::Result<()> {
            self.failure(user)?;
            self.secrets.borrow_mut().remove(user).map(|_| ()).ok_or(Error::NoEntry)
        }
    }

    #[test]
    fn machine_store_is_selected_only_for_an_unattended_helper() {
        assert_eq!(secret_store_kind(true, false), SecretStoreKind::MachineProtected);
        assert_eq!(secret_store_kind(true, true), SecretStoreKind::UserKeyring);
        assert_eq!(secret_store_kind(false, false), SecretStoreKind::UserKeyring);
    }

    fn settings(enabled: bool, password_set: bool, twofa: bool) -> Config {
        let mut config = Config::default();
        config.unattended_enabled = enabled;
        config.unattended_password_set = password_set;
        config.unattended_twofa_enabled = twofa;
        config
    }

    fn locked() -> Error {
        Error::NoStorageAccess("keychain is locked".into())
    }
    fn no_service() -> Error {
        Error::NoDefaultStore
    }
    fn platform_failure() -> Error {
        Error::PlatformFailure("org.freedesktop.DBus.Error.ServiceUnknown".into())
    }

    #[test]
    fn otpauth_uri_labels_account_with_device_id() {
        let uri = otpauth_uri("123456789", "ABCDEF");
        assert!(uri.starts_with("otpauth://totp/OpenDesk:123456789?secret=ABCDEF&issuer=OpenDesk"));
    }

    #[test]
    fn qr_modules_are_square() {
        let (width, modules) = qr_modules(&otpauth_uri("123456789", "JBSWY3DPEHPK3PXP")).unwrap();
        assert_eq!(modules.len(), width * width);
        assert!(modules.iter().any(|dark| *dark));
    }

    #[test]
    fn ready_with_every_secret_it_needs() {
        let store = FakeStore::with(&[(USER, "long-password-1"), (TOTP_USER, "JBSWY3DPEHPK3PXP")]);
        let check = check(&store, true, true, true);
        assert!(check.ready());
        assert!(check.has_password);
        assert_eq!(check.password.as_deref(), Some("long-password-1"));
        assert_eq!(check.totp_secret.as_deref(), Some("JBSWY3DPEHPK3PXP"));
        assert_eq!(check.problem, None);
    }

    #[test]
    fn a_wiped_password_is_not_ready_even_when_the_config_says_set() {
        let check = check(&FakeStore::default(), true, true, false);
        assert!(!check.ready());
        assert!(!check.has_password, "the marker must not hide a missing secret");
        assert!(check.problem.unwrap().contains("Enter the password again"));
    }

    #[test]
    fn a_wiped_2fa_secret_is_not_ready() {
        let store = FakeStore::with(&[(USER, "long-password-1")]);
        let check = check(&store, true, true, true);
        assert!(!check.ready());
        assert!(check.has_password);
        assert!(check.problem.unwrap().contains("Require 2FA code"));
    }

    #[test]
    fn a_locked_keyring_is_not_ready_and_says_why() {
        for fail in [locked as fn() -> Error, no_service, platform_failure] {
            let check = check(&FakeStore::broken(&[USER], fail), true, true, false);
            assert!(!check.ready());
            assert!(check.has_password, "falls back to the marker when the keyring can't answer");
            let problem = check.problem.unwrap();
            assert!(problem.contains("system keyring"), "{problem}");
            assert!(problem.contains("approval"), "{problem}");
            assert!(problem.ends_with("then try again."), "{problem}");
        }
    }

    #[test]
    fn an_unreadable_2fa_secret_is_not_ready() {
        let mut store = FakeStore::broken(&[TOTP_USER], locked);
        store.secrets.get_mut().insert(USER.into(), "long-password-1".into());
        let check = check(&store, true, true, true);
        assert!(!check.ready());
        assert!(check.password.is_none(), "must not run unattended without the 2FA secret");
        assert!(check.problem.unwrap().contains("2FA secret"));
    }

    #[test]
    fn disabled_reports_no_problem() {
        assert_eq!(check(&FakeStore::broken(&[USER], no_service), false, false, false), Check::default());
        let check = check(&FakeStore::broken(&[USER], no_service), false, true, true);
        assert_eq!(check, Check { has_password: true, ..Check::default() });
    }

    #[test]
    fn save_failures_explain_what_to_do() {
        let store = FakeStore::broken(&[USER, TOTP_USER], no_service);
        let error = save_password(&store, "long-password-1").unwrap_err();
        assert!(error.starts_with("OpenDesk couldn't find a system keyring to save the unattended access password."), "{error}");
        let error = save_totp_secret(&FakeStore::broken(&[TOTP_USER], locked), "SECRET").unwrap_err();
        assert!(error.starts_with("OpenDesk couldn't save the 2FA secret to the system keyring."), "{error}");
    }

    #[test]
    fn enabling_fails_with_the_keyring_message_and_leaves_config_alone() {
        let mut config = Config::default();
        let error = set_access(&FakeStore::broken(&[USER], no_service), &mut config, true, Some("long-password-1")).unwrap_err();
        assert!(error.contains("system keyring"), "{error}");
        assert!(!config.unattended_enabled);
        assert!(!config.unattended_password_set);

        let error = set_access(&FakeStore::broken(&[USER], locked), &mut config, true, None).unwrap_err();
        assert!(error.contains("system keyring"), "{error}");
        assert!(!config.unattended_enabled);
    }

    #[test]
    fn enabling_saves_and_marks_the_password() {
        let store = FakeStore::default();
        let mut config = Config::default();
        assert_eq!(
            set_access(&store, &mut config, true, None),
            Err("enter an unattended access password before enabling it".into())
        );
        set_access(&store, &mut config, true, Some("long-password-1")).unwrap();
        assert!(config.unattended_enabled && config.unattended_password_set);
        assert!(check(&store, true, true, false).ready());
    }

    #[test]
    fn turning_off_works_with_a_broken_keyring() {
        let mut config = settings(true, true, false);
        set_access(&FakeStore::broken(&[USER], locked), &mut config, false, None).unwrap();
        assert!(!config.unattended_enabled);
        assert!(config.unattended_password_set, "unknown, so the marker stays");

        let mut config = settings(true, true, false);
        set_access(&FakeStore::default(), &mut config, false, None).unwrap();
        assert!(!config.unattended_password_set, "a wiped password clears the marker");
    }

    #[test]
    fn twofa_can_be_turned_off_after_the_keyring_lost_its_secret() {
        let store = FakeStore::with(&[(USER, "long-password-1")]);
        let mut config = settings(true, false, true);
        assert!(!twofa_active(&store, &config), "so enrolling again is allowed");
        disable_twofa(&store, &mut config).unwrap();
        assert!(!config.unattended_twofa_enabled);
    }

    #[test]
    fn twofa_stays_on_when_the_keyring_cannot_remove_it() {
        let mut config = settings(false, false, true);
        assert!(disable_twofa(&FakeStore::broken(&[TOTP_USER], locked), &mut config).is_err());
        assert!(config.unattended_twofa_enabled);
        // Cancelling an enrollment when 2FA was never on doesn't touch it.
        let mut config = Config::default();
        assert_eq!(disable_twofa(&FakeStore::broken(&[TOTP_USER], locked), &mut config), Ok(()));
    }

    #[test]
    fn twofa_is_only_turned_on_once_its_secret_is_saved() {
        let mut config = Config::default();
        assert!(enable_twofa(&FakeStore::broken(&[TOTP_USER], no_service), &mut config, "SECRET").is_err());
        assert!(!config.unattended_twofa_enabled);
        let store = FakeStore::default();
        enable_twofa(&store, &mut config, "SECRET").unwrap();
        assert!(config.unattended_twofa_enabled && twofa_active(&store, &config));
    }

    #[test]
    fn saved_device_passwords_round_trip_and_are_namespaced() {
        let store = FakeStore::default();
        assert!(!has_contact_password(&store, "123456789"));
        save_contact_password(&store, "123456789", "hunter2").unwrap();
        assert!(has_contact_password(&store, "123456789"));
        assert_eq!(read_contact_password(&store, "123456789"), Lookup::Found("hunter2".into()));
        // A different device has its own entry, and the unattended user is untouched.
        assert!(!has_contact_password(&store, "987654321"));
        assert_eq!(read_password(&store), Lookup::Missing);
        delete_contact_password(&store, "123456789").unwrap();
        assert!(!has_contact_password(&store, "123456789"));
        // Forgetting one that is already gone is fine.
        assert_eq!(delete_contact_password(&store, "123456789"), Ok(()));
    }

    #[test]
    fn deleting_a_missing_2fa_secret_succeeds() {
        assert_eq!(delete_totp_secret(&FakeStore::default()), Ok(()));
        let store = FakeStore::with(&[(TOTP_USER, "SECRET")]);
        assert_eq!(delete_totp_secret(&store), Ok(()));
        assert_eq!(read_totp_secret(&store), Lookup::Missing);
        let error = delete_totp_secret(&FakeStore::broken(&[TOTP_USER], locked)).unwrap_err();
        assert!(error.starts_with("OpenDesk couldn't remove the 2FA secret from the system keyring."), "{error}");
    }
}
