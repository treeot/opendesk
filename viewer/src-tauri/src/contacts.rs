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
        let contact = Contact { id, alias, last_connected: None, mac: None, has_saved_password: false };
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
