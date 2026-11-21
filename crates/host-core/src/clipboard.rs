use protocol::{
    clipboard::{
        valid_file_name, ClipboardContent, ClipboardFile, ClipboardMsg, MAX_CLIPBOARD,
        MAX_CLIPBOARD_FILES, MAX_CLIPBOARD_IMAGE,
    },
    datamsg::DataMsg,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub trait ClipboardBackend {
    fn read(&self) -> Option<ClipboardContent>;
    fn write(&self, content: &ClipboardContent);
    /// Files or folders copied in the OS file manager, if any.
    fn read_files(&self) -> Option<Vec<PathBuf>> {
        None
    }
    /// Put local files on the clipboard so the OS file manager can paste them.
    fn write_files(&self, _paths: &[PathBuf]) {}
}

/// Files or folders on this computer's clipboard right now, or an empty list.
/// Used when the user pastes them into the remote computer.
pub fn local_file_list() -> Vec<PathBuf> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get().file_list())
        .unwrap_or_default()
}

/// The local paths behind the file list this side last announced, so a
/// peer's paste can name an entry by index without ever seeing the path.
/// Cleared as soon as the clipboard holds something else.
#[derive(Debug, Clone, Default)]
pub struct SharedFiles(Arc<Mutex<Option<Announced>>>);

/// An announcement's sequence number and the paths behind its entries.
type Announced = (u64, Vec<PathBuf>);

impl SharedFiles {
    /// Entry `index` of announcement `seq`, if that is still what is shared.
    pub fn path(&self, seq: u64, index: u32) -> Option<PathBuf> {
        let shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let (current, paths) = shared.as_ref()?;
        if *current != seq {
            return None;
        }
        paths.get(usize::try_from(index).ok()?).cloned()
    }
    /// Stop sharing, for example while clipboard sync is off.
    pub fn clear(&self) {
        self.set(None);
    }
    fn set(&self, value: Option<Announced>) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = value;
    }
}

/// Describe local paths for a file-list announcement: names and sizes only.
/// Entries that no longer exist or whose names cannot travel are left out,
/// and the kept paths line up with the returned entries. `None` when nothing
/// is left or the list is over the limit.
pub fn describe_files(paths: &[PathBuf]) -> Option<(Vec<ClipboardFile>, Vec<PathBuf>)> {
    if paths.len() > MAX_CLIPBOARD_FILES {
        return None;
    }
    let mut files = Vec::new();
    let mut kept = Vec::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !valid_file_name(name) {
            continue;
        }
        files.push(ClipboardFile {
            name: name.to_owned(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            is_dir: meta.is_dir(),
        });
        kept.push(path.clone());
    }
    (!files.is_empty()).then_some((files, kept))
}

pub struct ArboardBackend {
    clipboard: std::cell::RefCell<arboard::Clipboard>,
}
impl ArboardBackend {
    pub fn new() -> Option<Self> {
        arboard::Clipboard::new().ok().map(|clipboard| Self {
            clipboard: std::cell::RefCell::new(clipboard),
        })
    }
}
impl ClipboardBackend for ArboardBackend {
    fn read_files(&self) -> Option<Vec<PathBuf>> {
        let mut clipboard = self.clipboard.borrow_mut();
        clipboard
            .get()
            .file_list()
            .ok()
            .filter(|paths| !paths.is_empty())
    }
    fn write_files(&self, paths: &[PathBuf]) {
        let mut clipboard = self.clipboard.borrow_mut();
        let _ = clipboard.set().file_list(paths);
    }
    fn read(&self) -> Option<ClipboardContent> {
        let mut clipboard = self.clipboard.borrow_mut();
        if let Ok(image) = clipboard.get_image() {
            if image.width == 0
                || image.height == 0
                || image.width > 16_384
                || image.height > 16_384
                || image.width.saturating_mul(image.height) > 100_000_000
            {
                return None;
            }
            let width = u32::try_from(image.width).ok()?;
            let height = u32::try_from(image.height).ok()?;
            let rgba = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())?;
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(rgba)
                .write_to(&mut png, image::ImageFormat::Png)
                .ok()?;
            let png = png.into_inner();
            if png.len() <= MAX_CLIPBOARD_IMAGE {
                return Some(ClipboardContent::Image { width, height, png });
            }
        }
        clipboard
            .get_text()
            .ok()
            .filter(|text| text.len() <= MAX_CLIPBOARD)
            .map(ClipboardContent::Text)
    }
    fn write(&self, content: &ClipboardContent) {
        let mut clipboard = self.clipboard.borrow_mut();
        match content {
            ClipboardContent::Text(text) if text.len() <= MAX_CLIPBOARD => {
                let _ = clipboard.set_text(text.clone());
            }
            ClipboardContent::Image { width, height, png }
                if png.len() <= MAX_CLIPBOARD_IMAGE && *width > 0 && *height > 0 =>
            {
                if let Ok(decoded) =
                    image::load_from_memory_with_format(png, image::ImageFormat::Png)
                {
                    if decoded.width() == *width && decoded.height() == *height {
                        let rgba = decoded.to_rgba8();
                        let _ = clipboard.set_image(arboard::ImageData {
                            width: *width as usize,
                            height: *height as usize,
                            bytes: rgba.into_raw().into(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

pub struct ClipboardSync<B: ClipboardBackend> {
    backend: B,
    last_seen: Option<ClipboardContent>,
    last_hash: Option<u64>,
    next_seq: u64,
    /// Set when copied files may be announced to the peer (the host side).
    files: Option<SharedFiles>,
}
impl<B: ClipboardBackend> ClipboardSync<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            last_seen: None,
            last_hash: None,
            next_seq: 0,
            files: None,
        }
    }
    /// Announce copied files to the peer and record their paths in `shared`.
    /// Without it copied files are never announced, and their file names are
    /// not sent as text either.
    pub fn with_file_sharing(mut self, shared: SharedFiles) -> Self {
        self.files = Some(shared);
        self
    }
    /// Put files received from the peer on the local clipboard without
    /// announcing them back.
    pub fn apply_files(&mut self, paths: &[PathBuf]) {
        self.backend.write_files(paths);
        if let Some((files, _)) = describe_files(paths) {
            let content = ClipboardContent::Files(files);
            self.last_hash = Some(hash(&content));
            self.last_seen = Some(content);
        }
        if let Some(shared) = &self.files {
            shared.clear();
        }
    }
    pub fn backend(&self) -> &B {
        &self.backend
    }
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
    pub fn apply(&mut self, content: &ClipboardContent) {
        self.backend.write(content);
        self.last_hash = Some(hash(content));
    }
    pub fn poll(&mut self) -> Option<ClipboardMsg> {
        let local_files = self
            .backend
            .read_files()
            .and_then(|paths| describe_files(&paths));
        let (content, paths) = match local_files {
            Some((files, paths)) => (ClipboardContent::Files(files), Some(paths)),
            None => (self.backend.read()?, None),
        };
        if self.last_seen.is_none() {
            self.last_seen = Some(content);
            return None;
        }
        if self.last_seen.as_ref() == Some(&content) {
            return None;
        }
        self.last_seen = Some(content.clone());
        if let Some(shared) = &self.files {
            shared.clear();
        }
        if self.last_hash == Some(hash(&content)) {
            return None;
        }
        let paths = match (paths, &self.files) {
            (Some(paths), Some(_)) => Some(paths),
            (Some(_), None) => return None,
            (None, _) => None,
        };
        if matches!(&content, ClipboardContent::Text(t) if t.len() > MAX_CLIPBOARD)
            || matches!(&content, ClipboardContent::Image { png, .. } if png.len() > MAX_CLIPBOARD_IMAGE)
        {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if let (Some(paths), Some(shared)) = (paths, &self.files) {
            shared.set(Some((seq, paths)));
        }
        Some(ClipboardMsg { seq, content })
    }
    pub fn poll_data(&mut self) -> Option<DataMsg> {
        self.poll().map(DataMsg::Clipboard)
    }
}
fn hash(content: &ClipboardContent) -> u64 {
    let bytes: Vec<u8> = match content {
        ClipboardContent::Text(t) => t.as_bytes().to_vec(),
        ClipboardContent::Image { width, height, png } => [
            width.to_be_bytes().as_slice(),
            height.to_be_bytes().as_slice(),
            png,
        ]
        .concat(),
        ClipboardContent::Files(files) => files
            .iter()
            .flat_map(|file| {
                [
                    &[u8::from(file.is_dir)],
                    file.size.to_be_bytes().as_slice(),
                    file.name.as_bytes(),
                    &[0],
                ]
                .concat()
            })
            .collect(),
    };
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
#[derive(Debug, Default)]
pub struct FakeBackend {
    content: std::cell::RefCell<Option<ClipboardContent>>,
    files: std::cell::RefCell<Option<Vec<PathBuf>>>,
}
impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        *self.content.get_mut() = Some(ClipboardContent::Text(text.into()));
        *self.files.get_mut() = None;
    }
    pub fn set_files(&mut self, paths: Vec<PathBuf>) {
        *self.files.get_mut() = Some(paths);
    }
    pub fn files(&self) -> Option<Vec<PathBuf>> {
        self.files.borrow().clone()
    }
}
impl ClipboardBackend for FakeBackend {
    fn read(&self) -> Option<ClipboardContent> {
        self.content.borrow().clone()
    }
    fn write(&self, content: &ClipboardContent) {
        *self.content.borrow_mut() = Some(content.clone());
        *self.files.borrow_mut() = None;
    }
    fn read_files(&self) -> Option<Vec<PathBuf>> {
        self.files.borrow().clone()
    }
    fn write_files(&self, paths: &[PathBuf]) {
        *self.files.borrow_mut() = Some(paths.to_vec());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_text_change_detected() {
        let mut b = FakeBackend::new();
        b.set_text("hello");
        let mut s = ClipboardSync::new(b);
        assert_eq!(s.poll(), None);
        s.backend_mut().set_text("world");
        assert_eq!(s.poll(), Some(ClipboardMsg::text(0, "world".into())));
    }
    #[test]
    fn local_image_change_detected_and_echo_suppressed() {
        let mut b = FakeBackend::new();
        b.content.get_mut().replace(ClipboardContent::Image {
            width: 1,
            height: 1,
            png: vec![1],
        });
        let mut s = ClipboardSync::new(b);
        assert_eq!(s.poll(), None);
        let image = ClipboardContent::Image {
            width: 2,
            height: 3,
            png: vec![4, 5],
        };
        s.backend().write(&image);
        assert_eq!(
            s.poll(),
            Some(ClipboardMsg {
                seq: 0,
                content: image.clone()
            })
        );
        s.apply(&image);
        assert_eq!(s.poll(), None);
    }
    fn temp_files() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "opendesk-clip-{}-{}",
            std::process::id(),
            crate::filexfer::next_transfer_id()
        ));
        let file = root.join("notes.txt");
        let dir = root.join("photos");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, b"hello").unwrap();
        (root, file, dir)
    }
    #[test]
    fn copied_files_are_announced_by_name_and_resolved_by_index() {
        let (root, file, dir) = temp_files();
        let mut b = FakeBackend::new();
        b.set_text("before");
        let shared = SharedFiles::default();
        let mut s = ClipboardSync::new(b).with_file_sharing(shared.clone());
        assert_eq!(s.poll(), None);
        s.backend_mut()
            .set_files(vec![file.clone(), root.join("gone.bin"), dir.clone()]);
        let msg = s.poll().unwrap();
        assert_eq!(
            msg.content,
            ClipboardContent::Files(vec![
                ClipboardFile {
                    name: "notes.txt".into(),
                    size: 5,
                    is_dir: false
                },
                ClipboardFile {
                    name: "photos".into(),
                    size: 0,
                    is_dir: true
                },
            ])
        );
        // Only names travel; the paths stay here, looked up by index.
        let wire = String::from_utf8_lossy(&msg.to_bytes()).into_owned();
        assert!(!wire.contains(&root.display().to_string()));
        assert_eq!(shared.path(msg.seq, 0), Some(file));
        assert_eq!(shared.path(msg.seq, 1), Some(dir));
        assert_eq!(shared.path(msg.seq, 2), None);
        assert_eq!(shared.path(msg.seq + 1, 0), None);
        // Copying something else stops sharing the old list.
        s.backend_mut().set_text("after");
        assert!(s.poll().is_some());
        assert_eq!(shared.path(msg.seq, 0), None);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn copied_files_stay_private_without_file_sharing() {
        let (root, file, _) = temp_files();
        let mut b = FakeBackend::new();
        b.set_text("before");
        let mut s = ClipboardSync::new(b);
        assert_eq!(s.poll(), None);
        s.backend_mut().set_files(vec![file]);
        // Neither the list nor the file names as text are sent.
        assert_eq!(s.poll(), None);
        assert_eq!(s.poll(), None);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pasted_files_land_on_the_clipboard_without_echo() {
        let (root, file, dir) = temp_files();
        let mut b = FakeBackend::new();
        b.set_text("before");
        let shared = SharedFiles::default();
        let mut s = ClipboardSync::new(b).with_file_sharing(shared.clone());
        assert_eq!(s.poll(), None);
        let paths = vec![file, dir];
        s.apply_files(&paths);
        assert_eq!(s.backend().files(), Some(paths));
        assert_eq!(s.poll(), None);
        assert_eq!(shared.path(0, 0), None);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn oversized_file_lists_are_not_described() {
        let paths = vec![PathBuf::from("/x"); MAX_CLIPBOARD_FILES + 1];
        assert!(describe_files(&paths).is_none());
    }
}
