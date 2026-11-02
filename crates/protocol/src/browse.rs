use serde::{Deserialize, Serialize};

/// One entry in a remote directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    /// File size in bytes; 0 for directories.
    pub size: u64,
    /// Last-modified time, Unix seconds; 0 when unknown.
    pub modified: u64,
}

/// Remote file-manager messages on the Data channel. The viewer asks the host
/// to list a directory; the host answers with a listing or an error. `req`
/// correlates each answer with its request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum BrowseMsg {
    /// Viewer → host: list `path`. An empty path means the starting places
    /// (the user's home directory, and drive roots on Windows).
    ListDir { req: u64, path: String },
    /// Host → viewer: the entries of `path` (the absolute path the host read).
    DirListing {
        req: u64,
        path: String,
        entries: Vec<DirEntry>,
    },
    /// Host → viewer: `path` could not be listed.
    BrowseError { req: u64, message: String },
    /// Viewer → host: send me the file at `path`. The host answers by starting a
    /// normal file transfer (an Offer) for it, which the viewer receives like
    /// any other incoming file.
    Pull { req: u64, path: String },
    /// Viewer → host: save the next uploaded file into `dir` instead of the
    /// host's Downloads. Consumed by the next incoming transfer, so it is sent
    /// right before the file. An empty `dir` means Downloads.
    SetUploadDir { dir: String },
    /// Viewer → host: create a directory inside `path`.
    MakeDir {
        req: u64,
        path: String,
        name: String,
    },
    /// Viewer → host: remove a file or directory at `path`.
    Delete { req: u64, path: String },
    /// Viewer → host: rename a file or directory. `name` is a single component.
    Rename {
        req: u64,
        path: String,
        name: String,
    },
}

impl BrowseMsg {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("browse message serializes")
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<BrowseMsg> {
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_messages_roundtrip() {
        for message in [
            BrowseMsg::ListDir {
                req: 1,
                path: "/home/me".into(),
            },
            BrowseMsg::DirListing {
                req: 1,
                path: "/home/me".into(),
                entries: vec![DirEntry {
                    name: "docs".into(),
                    is_dir: true,
                    size: 0,
                    modified: 100,
                }],
            },
            BrowseMsg::BrowseError {
                req: 2,
                message: "denied".into(),
            },
            BrowseMsg::Pull {
                req: 3,
                path: "/home/me/a.txt".into(),
            },
            BrowseMsg::SetUploadDir {
                dir: "/home/me/docs".into(),
            },
            BrowseMsg::MakeDir {
                req: 4,
                path: "/home/me".into(),
                name: "new".into(),
            },
            BrowseMsg::Delete {
                req: 5,
                path: "/home/me/old".into(),
            },
            BrowseMsg::Rename {
                req: 6,
                path: "/home/me/old".into(),
                name: "new".into(),
            },
        ] {
            let bytes = message.to_bytes();
            assert_eq!(BrowseMsg::from_bytes(&bytes), Some(message.clone()));
            assert_eq!(bytes, message.to_bytes());
        }
    }
}
