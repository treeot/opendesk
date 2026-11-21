use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub alias: String,
    pub last_connected: Option<u64>,
    /// The device's MAC address, if set, used to wake it over the local network
    /// (Wake-on-LAN). Stored as the user typed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Free-form notes about the device.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// Tags used to filter and group devices. A `/` nests one under another,
    /// so "Clients/Acme" also shows up under "Clients".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Pinned to the top of the address book.
    #[serde(default, skip_serializing_if = "is_false")]
    pub favorite: bool,
    /// Whether a password is saved for this device in the OS keyring. Runtime
    /// only: filled in when listing contacts, never written to contacts.json.
    #[serde(default, skip_serializing)]
    pub has_saved_password: bool,
}

#[derive(Debug, Clone)]
pub struct ContactStore {
    path: PathBuf,
    contacts: Vec<Contact>,
}

impl ContactStore {
    pub fn load(app_data_dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = app_data_dir.as_ref();
        fs::create_dir_all(dir)?;
        let path = dir.join("contacts.json");
        let contacts = match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(contacts) => contacts,
                Err(error) => {
                    let backup = crate::config::backup_corrupt_file(&path)?;
                    log::warn!(
                        "could not parse contacts file ({error}); backed it up to {}",
                        backup.display()
                    );
                    Vec::new()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let store = Self { path, contacts };
        store.save()?;
        Ok(store)
    }

    pub fn list(&self) -> Vec<Contact> {
        self.contacts.clone()
    }

    pub fn add(&mut self, alias: String, id: String) -> io::Result<Contact> {
        let alias = alias.trim().to_string();
        let id = id.trim().to_string();
        if alias.is_empty() || id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "contact alias and peer ID cannot be empty",
            ));
        }
        if let Some(contact) = self.contacts.iter_mut().find(|contact| contact.id == id) {
            contact.alias = alias;
            let contact = contact.clone();
            self.save()?;
            return Ok(contact);
        }
        let contact = Contact {
            id,
            alias,
            last_connected: None,
            mac: None,
            notes: String::new(),
            tags: Vec::new(),
            favorite: false,
            has_saved_password: false,
        };
        self.contacts.push(contact.clone());
        self.save()?;
        Ok(contact)
    }

    /// Set (or clear, with an empty string) the MAC address used to wake a
    /// contact over the local network. Returns the updated contact.
    pub fn set_mac(&mut self, id: &str, mac: &str) -> io::Result<Contact> {
        let contact = self
            .contacts
            .iter_mut()
            .find(|contact| contact.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "contact not found"))?;
        let mac = mac.trim();
        contact.mac = if mac.is_empty() { None } else { Some(mac.to_string()) };
        let contact = contact.clone();
        self.save()?;
        Ok(contact)
    }

    /// The MAC address saved for a contact, if any.
    pub fn mac(&self, id: &str) -> Option<String> {
        self.contacts.iter().find(|contact| contact.id == id).and_then(|c| c.mac.clone())
    }

    /// Replace a contact's editable details. Returns the updated contact.
    pub fn update(&mut self, id: &str, edit: ContactEdit) -> io::Result<Contact> {
        let alias = edit.alias.trim().to_string();
        if alias.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "contact name cannot be empty"));
        }
        let tags = normalize_tags(&edit.tags).map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
        let contact = self
            .contacts
            .iter_mut()
            .find(|contact| contact.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "contact not found"))?;
        contact.alias = alias;
        contact.notes = edit.notes.trim().to_string();
        contact.tags = tags;
        contact.favorite = edit.favorite;
        let contact = contact.clone();
        self.save()?;
        Ok(contact)
    }

    /// Merge imported contacts into the store and save. Contacts are matched
    /// by device ID; see [`merge_contacts`] for how fields combine.
    pub fn merge(&mut self, incoming: Vec<Contact>) -> io::Result<MergeReport> {
        let mut merged = self.contacts.clone();
        let report = merge_contacts(&mut merged, incoming);
        if report.added > 0 || report.updated > 0 {
            let previous = std::mem::replace(&mut self.contacts, merged);
            if let Err(error) = self.save() {
                self.contacts = previous;
                return Err(error);
            }
        }
        Ok(report)
    }

    pub fn remove(&mut self, id: &str) -> io::Result<()> {
        self.contacts.retain(|contact| contact.id != id);
        self.save()
    }

    pub fn touch(&mut self, id: &str) -> io::Result<()> {
        let contact = self
            .contacts
            .iter_mut()
            .find(|contact| contact.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "contact not found"))?;
        contact.last_connected = Some(unix_seconds());
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.contacts).map_err(io::Error::other)?;
        crate::config::write_atomic(&self.path, &bytes)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The details the user can edit on a saved device.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContactEdit {
    pub alias: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub favorite: bool,
}

const MAX_TAG_LEN: usize = 64;
const MAX_TAGS: usize = 32;

/// Trim tags and the parts between `/`, then drop empty ones and duplicates
/// (case-insensitively, keeping the first spelling).
fn normalize_tags(tags: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.split('/').map(str::trim).filter(|part| !part.is_empty()).collect::<Vec<_>>().join("/");
        if tag.is_empty() {
            continue;
        }
        if tag.chars().count() > MAX_TAG_LEN {
            return Err(format!("tag \"{tag}\" is longer than {MAX_TAG_LEN} characters"));
        }
        if !out.iter().any(|existing| existing.to_lowercase() == tag.to_lowercase()) {
            out.push(tag);
        }
    }
    if out.len() > MAX_TAGS {
        return Err(format!("a device can have at most {MAX_TAGS} tags"));
    }
    Ok(out)
}

/// What an import changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MergeReport {
    /// Devices that were not in the address book before.
    pub added: usize,
    /// Devices already saved that gained notes, tags, a MAC or similar.
    pub updated: usize,
    /// Devices already saved with nothing new in the file.
    pub unchanged: usize,
}

/// Merge `incoming` into `existing`, matched by device ID. New devices are
/// appended. For a device that is already saved nothing local is lost: its
/// name and MAC are kept (the file's only fill them in when missing), tags
/// are combined, differing notes are appended, it stays a favorite if either
/// side says so, and the later last-connected time wins.
pub fn merge_contacts(existing: &mut Vec<Contact>, incoming: Vec<Contact>) -> MergeReport {
    let mut report = MergeReport::default();
    let mut added_ids: Vec<String> = Vec::new();
    for contact in incoming {
        let Some(current) = existing.iter_mut().find(|item| item.id == contact.id) else {
            added_ids.push(contact.id.clone());
            existing.push(Contact { has_saved_password: false, ..contact });
            report.added += 1;
            continue;
        };
        let before = current.clone();
        if current.alias.trim().is_empty() {
            current.alias = contact.alias;
        }
        if current.mac.is_none() {
            current.mac = contact.mac;
        }
        let notes = contact.notes.trim();
        if current.notes.trim().is_empty() {
            current.notes = notes.to_string();
        } else if !notes.is_empty() && !current.notes.contains(notes) {
            current.notes = format!("{}\n\n{notes}", current.notes.trim_end());
        }
        let mut tags = current.tags.clone();
        tags.extend(contact.tags);
        current.tags = normalize_tags(&tags).unwrap_or(tags);
        current.favorite |= contact.favorite;
        current.last_connected = current.last_connected.max(contact.last_connected);
        // A device listed twice in one file was already counted as added.
        if added_ids.contains(&current.id) {
            continue;
        }
        if *current != before {
            report.updated += 1;
        } else {
            report.unchanged += 1;
        }
    }
    report
}

/// Marks a file as an OpenDesk address book export.
pub const EXPORT_FORMAT: &str = "opendesk-address-book";
/// The newest export format version this build understands.
pub const EXPORT_VERSION: u32 = 1;
/// Larger files are refused before parsing.
pub const MAX_IMPORT_BYTES: u64 = 5 * 1024 * 1024;

/// An exported address book.
#[derive(Debug, Clone, Serialize)]
pub struct AddressBookFile {
    pub format: String,
    pub version: u32,
    pub exported_at: u64,
    pub contacts: Vec<ExportedContact>,
}

/// One device in an export. `password` is only present when the user chose
/// to include saved passwords.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedContact {
    pub id: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Build an export of `contacts`. `password_for` is only consulted when
/// passwords are included; an `Err` from it (the keyring failed) fails the
/// whole export rather than quietly leaving passwords out.
pub fn build_export(
    contacts: &[Contact],
    include_passwords: bool,
    mut password_for: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<AddressBookFile, String> {
    let mut exported = Vec::with_capacity(contacts.len());
    for contact in contacts {
        let password = if include_passwords { password_for(&contact.id)? } else { None };
        exported.push(ExportedContact {
            id: contact.id.clone(),
            alias: contact.alias.clone(),
            notes: contact.notes.clone(),
            tags: contact.tags.clone(),
            favorite: contact.favorite,
            mac: contact.mac.clone(),
            last_connected: contact.last_connected,
            password,
        });
    }
    Ok(AddressBookFile { format: EXPORT_FORMAT.into(), version: EXPORT_VERSION, exported_at: unix_seconds(), contacts: exported })
}

/// A validated import: the contacts to merge and any passwords the file
/// carried, keyed by device ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedImport {
    pub contacts: Vec<Contact>,
    pub passwords: Vec<(String, String)>,
}

/// Parse and validate an address book file: an OpenDesk export or a raw
/// `contacts.json` list. Any invalid entry rejects the whole file with a
/// message naming it, so nothing is half imported or silently skipped.
pub fn parse_import(text: &str) -> Result<ParsedImport, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("This file is not valid JSON ({error})."))?;
    let entries = match value {
        serde_json::Value::Array(entries) => entries,
        serde_json::Value::Object(mut object) => {
            if object.get("format").and_then(|format| format.as_str()) != Some(EXPORT_FORMAT) {
                return Err("This file is not an OpenDesk address book.".into());
            }
            match object.get("version").and_then(|version| version.as_u64()) {
                Some(version) if (1..=u64::from(EXPORT_VERSION)).contains(&version) => {}
                Some(version) if version > u64::from(EXPORT_VERSION) => {
                    return Err("This address book was exported by a newer version of OpenDesk. Update OpenDesk to import it.".into());
                }
                _ => return Err("This address book has a missing or invalid version.".into()),
            }
            match object.remove("contacts") {
                Some(serde_json::Value::Array(entries)) => entries,
                _ => return Err("This address book has no device list.".into()),
            }
        }
        _ => return Err("This file is not an OpenDesk address book.".into()),
    };
    let mut parsed = ParsedImport { contacts: Vec::with_capacity(entries.len()), passwords: Vec::new() };
    for (index, entry) in entries.into_iter().enumerate() {
        let number = index + 1;
        let entry: ExportedContact = serde_json::from_value(entry)
            .map_err(|error| format!("Device {number} in the file is malformed ({error})."))?;
        let id: String = entry.id.chars().filter(|c| !c.is_whitespace()).collect();
        if id.len() != 9 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("Device {number} in the file has an invalid ID \"{}\". IDs are 9 digits.", entry.id));
        }
        let alias = entry.alias.trim();
        let alias = if alias.is_empty() { id.clone() } else { alias.to_string() };
        let mac = match entry.mac.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(mac) if host_core::wol::parse_mac(mac).is_some() => Some(mac.to_string()),
            Some(mac) => return Err(format!("Device {number} ({alias}) has an invalid MAC address \"{mac}\".")),
        };
        let tags = normalize_tags(&entry.tags).map_err(|message| format!("Device {number} ({alias}): {message}."))?;
        if let Some(password) = entry.password.filter(|password| !password.is_empty()) {
            parsed.passwords.retain(|(existing, _)| *existing != id);
            parsed.passwords.push((id.clone(), password));
        }
        parsed.contacts.push(Contact {
            id,
            alias,
            last_connected: entry.last_connected,
            mac,
            notes: entry.notes.trim().to_string(),
            tags,
            favorite: entry.favorite,
            has_saved_password: false,
        });
    }
    Ok(parsed)
}

/// What importing passwords into the keyring did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PasswordImport {
    /// Saved for devices that had no password yet.
    pub saved: usize,
    /// Left alone because a different password was already saved.
    pub kept_existing: usize,
    /// Could not be saved (keyring error).
    pub failed: usize,
}

/// Save imported passwords into the keyring. A password already saved for a
/// device is never overwritten.
pub fn import_passwords(store: &impl crate::unattended::SecretStore, passwords: &[(String, String)]) -> PasswordImport {
    use crate::unattended::{read_contact_password, save_contact_password, Lookup};
    let mut result = PasswordImport::default();
    for (id, password) in passwords {
        match read_contact_password(store, id) {
            Lookup::Found(existing) if existing == *password => {}
            Lookup::Found(_) => result.kept_existing += 1,
            Lookup::Missing => match save_contact_password(store, id, password) {
                Ok(()) => result.saved += 1,
                Err(_) => result.failed += 1,
            },
            Lookup::Unavailable(_) => result.failed += 1,
        }
    }
    result
}

/// Summary of an import, shown to the user.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportSummary {
    #[serde(flatten)]
    pub merge: MergeReport,
    pub passwords: PasswordImport,
}

#[tauri::command]
pub(crate) fn update_contact(state: tauri::State<'_, crate::AppState>, peer_id: String, edit: ContactEdit) -> Result<Contact, String> {
    state.lock().contacts.update(&peer_id, edit).map_err(|e| e.to_string())
}

/// Write the address book to `path`. Saved passwords are only included when
/// `include_passwords` is set, and such a file is readable by its owner only.
#[tauri::command]
pub(crate) fn export_contacts(state: tauri::State<'_, crate::AppState>, path: String, include_passwords: bool) -> Result<usize, String> {
    use crate::unattended::{read_contact_password, Keyring, Lookup};
    let contacts = state.lock().contacts.list();
    let file = build_export(&contacts, include_passwords, |id| match read_contact_password(&Keyring, id) {
        Lookup::Found(password) => Ok(Some(password)),
        Lookup::Missing => Ok(None),
        Lookup::Unavailable(message) => Err(message),
    })?;
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    write_export(Path::new(&path), &bytes, include_passwords).map_err(|e| format!("Could not write {path}: {e}"))?;
    Ok(file.contacts.len())
}

fn write_export(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // `mode` only applies to new files; tighten an existing one first.
        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Read, validate and merge an address book file. Passwords in the file go
/// to the OS keyring, never into contacts.json.
#[tauri::command]
pub(crate) fn import_contacts(state: tauri::State<'_, crate::AppState>, path: String) -> Result<ImportSummary, String> {
    let size = fs::metadata(&path).map_err(|e| format!("Could not read {path}: {e}"))?.len();
    if size > MAX_IMPORT_BYTES {
        return Err("This file is too large to be an address book.".into());
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("Could not read {path}: {e}"))?;
    let parsed = parse_import(&text)?;
    let merge = state.lock().contacts.merge(parsed.contacts).map_err(|e| e.to_string())?;
    let passwords = import_passwords(&crate::unattended::Keyring, &parsed.passwords);
    Ok(ImportSummary { merge, passwords })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn test_store() -> (ContactStore, PathBuf) {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("opendesk-contacts-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        (ContactStore::load(&path).unwrap(), path)
    }

    #[test]
    fn add_deduplicates_by_peer_id_and_updates_alias() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        let updated = store.add("Work Mac".into(), "123456789".into()).unwrap();
        assert_eq!(updated.alias, "Work Mac");
        assert_eq!(store.list().len(), 1);
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn remove_deletes_contact() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        store.remove("123456789").unwrap();
        assert!(store.list().is_empty());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn touch_sets_timestamp() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        store.touch("123456789").unwrap();
        assert!(store.list()[0].last_connected.is_some());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn set_mac_sets_clears_and_persists() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        store.set_mac("123456789", "01:23:45:67:89:AB").unwrap();
        assert_eq!(store.mac("123456789").as_deref(), Some("01:23:45:67:89:AB"));
        // Survives a reload (it is serialized).
        assert_eq!(ContactStore::load(&path).unwrap().mac("123456789").as_deref(), Some("01:23:45:67:89:AB"));
        // An empty string clears it.
        store.set_mac("123456789", "  ").unwrap();
        assert_eq!(store.mac("123456789"), None);
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn json_round_trip_via_file() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        let loaded = ContactStore::load(&path).unwrap();
        assert_eq!(loaded.list(), store.list());
        let _ = fs::remove_dir_all(path);
    }

    fn contact(id: &str, alias: &str) -> Contact {
        Contact {
            id: id.into(),
            alias: alias.into(),
            last_connected: None,
            mac: None,
            notes: String::new(),
            tags: Vec::new(),
            favorite: false,
            has_saved_password: false,
        }
    }

    #[test]
    fn old_contacts_file_loads_with_defaults() {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("opendesk-contacts-old-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let old = r#"[{"id":"123456789","alias":"Office","last_connected":1700000000,"mac":"01:23:45:67:89:AB"},{"id":"987654321","alias":"Laptop","last_connected":null}]"#;
        fs::write(path.join("contacts.json"), old).unwrap();

        let store = ContactStore::load(&path).unwrap();

        let contacts = store.list();
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].last_connected, Some(1_700_000_000));
        assert_eq!(contacts[0].mac.as_deref(), Some("01:23:45:67:89:AB"));
        assert!(contacts[0].notes.is_empty() && contacts[0].tags.is_empty() && !contacts[0].favorite);
        // No backup was made: the old file is valid.
        assert!(fs::read_dir(&path).unwrap().all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("corrupt")));
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn new_fields_round_trip_and_are_omitted_when_empty() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        store.add("Spare".into(), "987654321".into()).unwrap();
        let edit = ContactEdit { alias: " Front desk ".into(), notes: "Ask Sam first".into(), tags: vec![" Clients / Acme ".into(), "clients/acme".into(), "".into()], favorite: true };
        let updated = store.update("123456789", edit).unwrap();
        assert_eq!(updated.alias, "Front desk");
        assert_eq!(updated.tags, vec!["Clients/Acme".to_string()]);
        let loaded = ContactStore::load(&path).unwrap();
        assert_eq!(loaded.list(), store.list());
        let raw = fs::read_to_string(path.join("contacts.json")).unwrap();
        assert_eq!(raw.matches("\"favorite\"").count(), 1);
        assert_eq!(raw.matches("\"notes\"").count(), 1);
        assert!(!raw.contains("has_saved_password"));
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn update_rejects_empty_name_and_unknown_device() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        let blank = ContactEdit { alias: "  ".into(), ..ContactEdit::default() };
        assert_eq!(store.update("123456789", blank).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let missing = ContactEdit { alias: "X".into(), ..ContactEdit::default() };
        assert_eq!(store.update("000000000", missing).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(store.list()[0].alias, "Office");
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn merge_dedupes_by_id_and_keeps_local_data() {
        let mut local = contact("123456789", "Office");
        local.notes = "Rack 2".into();
        local.tags = vec!["Work".into()];
        local.last_connected = Some(200);
        let mut existing = vec![local];
        let mut same = contact("123456789", "Renamed elsewhere");
        same.notes = "Rack 2".into();
        same.tags = vec!["work".into(), "Clients/Acme".into()];
        same.favorite = true;
        same.mac = Some("01:23:45:67:89:AB".into());
        same.last_connected = Some(100);
        let fresh = contact("987654321", "Laptop");
        let duplicate = Contact { notes: "second copy".into(), ..contact("987654321", "Laptop again") };

        let report = merge_contacts(&mut existing, vec![same, fresh, duplicate]);

        assert_eq!(report, MergeReport { added: 1, updated: 1, unchanged: 0 });
        assert_eq!(existing.len(), 2);
        let office = &existing[0];
        assert_eq!(office.alias, "Office");
        assert_eq!(office.notes, "Rack 2");
        assert_eq!(office.tags, vec!["Work".to_string(), "Clients/Acme".to_string()]);
        assert!(office.favorite);
        assert_eq!(office.mac.as_deref(), Some("01:23:45:67:89:AB"));
        assert_eq!(office.last_connected, Some(200));
        assert_eq!(existing[1].alias, "Laptop");
        assert_eq!(existing[1].notes, "second copy");
    }

    #[test]
    fn merge_appends_differing_notes_and_counts_unchanged() {
        let mut existing = vec![Contact { notes: "Local note".into(), ..contact("123456789", "Office") }];
        let report = merge_contacts(&mut existing, vec![Contact { notes: "Imported note".into(), ..contact("123456789", "Office") }]);
        assert_eq!(report.updated, 1);
        assert_eq!(existing[0].notes, "Local note\n\nImported note");
        let again = merge_contacts(&mut existing, vec![Contact { notes: "Imported note".into(), ..contact("123456789", "Office") }]);
        assert_eq!(again, MergeReport { added: 0, updated: 0, unchanged: 1 });
    }

    #[test]
    fn store_merge_persists() {
        let (mut store, path) = test_store();
        store.add("Office".into(), "123456789".into()).unwrap();
        let report = store.merge(vec![contact("987654321", "Laptop")]).unwrap();
        assert_eq!(report.added, 1);
        assert_eq!(ContactStore::load(&path).unwrap().list().len(), 2);
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn export_leaves_out_passwords_unless_asked() {
        let mut office = contact("123456789", "Office");
        office.tags = vec!["Home".into()];
        let contacts = vec![office];
        let mut asked = 0;
        let plain = build_export(&contacts, false, |_| { asked += 1; Ok(Some("secret".into())) }).unwrap();
        assert_eq!(asked, 0);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("secret"));
        assert!(json.contains(EXPORT_FORMAT));

        let with = build_export(&contacts, true, |_| Ok(Some("secret".into()))).unwrap();
        assert_eq!(with.contacts[0].password.as_deref(), Some("secret"));
        // A keyring failure fails the export instead of dropping passwords.
        assert!(build_export(&contacts, true, |_| Err("locked".into())).is_err());
    }

    #[test]
    fn export_then_import_round_trips() {
        let mut office = contact("123456789", "Office");
        office.notes = "Front desk".into();
        office.tags = vec!["Clients/Acme".into()];
        office.favorite = true;
        office.mac = Some("01:23:45:67:89:AB".into());
        office.last_connected = Some(42);
        let file = build_export(std::slice::from_ref(&office), true, |_| Ok(Some("pw".into()))).unwrap();
        let parsed = parse_import(&serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(parsed.contacts, vec![office]);
        assert_eq!(parsed.passwords, vec![("123456789".to_string(), "pw".to_string())]);
    }

    #[test]
    fn import_accepts_raw_contacts_json() {
        let parsed = parse_import(r#"[{"id":"123 456 789","alias":"","last_connected":null}]"#).unwrap();
        assert_eq!(parsed.contacts[0].id, "123456789");
        assert_eq!(parsed.contacts[0].alias, "123456789");
        assert!(parsed.passwords.is_empty());
    }

    #[test]
    fn import_rejects_malformed_files_clearly() {
        let error = |text: &str| parse_import(text).unwrap_err();
        assert!(error("{not json").contains("not valid JSON"));
        assert!(error(r#"{"hello":1}"#).contains("not an OpenDesk address book"));
        assert!(error(r#""text""#).contains("not an OpenDesk address book"));
        assert!(error(r#"{"format":"opendesk-address-book","version":99,"contacts":[]}"#).contains("newer version"));
        assert!(error(r#"{"format":"opendesk-address-book","contacts":[]}"#).contains("version"));
        assert!(error(r#"{"format":"opendesk-address-book","version":1}"#).contains("no device list"));
        assert!(error(r#"[{"id":"123456789"},{"id":"12345"}]"#).contains("Device 2"));
        assert!(error(r#"[{"alias":"No ID"}]"#).contains("Device 1 in the file is malformed"));
        assert!(error(r#"[{"id":"123456789","alias":"A","mac":"nope"}]"#).contains("invalid MAC"));
        assert!(error(r#"[{"id":"123456789","tags":"Home"}]"#).contains("malformed"));
    }

    #[derive(Default)]
    struct FakeKeyring(std::cell::RefCell<std::collections::HashMap<String, String>>);

    impl crate::unattended::SecretStore for FakeKeyring {
        fn get(&self, user: &str) -> keyring::Result<String> {
            self.0.borrow().get(user).cloned().ok_or(keyring::Error::NoEntry)
        }
        fn set(&self, user: &str, secret: &str) -> keyring::Result<()> {
            self.0.borrow_mut().insert(user.into(), secret.into());
            Ok(())
        }
        fn delete(&self, user: &str) -> keyring::Result<()> {
            self.0.borrow_mut().remove(user);
            Ok(())
        }
    }

    #[test]
    fn imported_passwords_go_to_keyring_without_overwriting() {
        let store = FakeKeyring::default();
        crate::unattended::save_contact_password(&store, "111111111", "local").unwrap();
        crate::unattended::save_contact_password(&store, "333333333", "same").unwrap();
        let passwords = vec![
            ("111111111".to_string(), "imported".to_string()),
            ("222222222".to_string(), "new".to_string()),
            ("333333333".to_string(), "same".to_string()),
        ];
        let result = import_passwords(&store, &passwords);
        assert_eq!(result, PasswordImport { saved: 1, kept_existing: 1, failed: 0 });
        assert_eq!(crate::unattended::read_contact_password(&store, "111111111"), crate::unattended::Lookup::Found("local".into()));
        assert_eq!(crate::unattended::read_contact_password(&store, "222222222"), crate::unattended::Lookup::Found("new".into()));
    }

    #[test]
    fn corrupt_contacts_are_backed_up_and_replaced_with_empty_list() {
        let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("opendesk-contacts-corrupt-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let original = b"[{\"id\": ";
        fs::write(path.join("contacts.json"), original).unwrap();

        let store = ContactStore::load(&path).unwrap();

        assert!(store.list().is_empty());
        let backups: Vec<_> = fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|file| file.file_name().unwrap().to_string_lossy().starts_with("contacts.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), original);
        let _ = fs::remove_dir_all(path);
    }
}
