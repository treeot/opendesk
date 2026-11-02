use protocol::{
    clipboard::{ClipboardContent, ClipboardMsg, MAX_CLIPBOARD, MAX_CLIPBOARD_IMAGE},
    datamsg::DataMsg,
};

pub trait ClipboardBackend {
    fn read(&self) -> Option<ClipboardContent>;
    fn write(&self, content: &ClipboardContent);
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
}
impl<B: ClipboardBackend> ClipboardSync<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            last_seen: None,
            last_hash: None,
            next_seq: 0,
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
        let content = self.backend.read()?;
        if self.last_seen.is_none() {
            self.last_seen = Some(content);
            return None;
        }
        if self.last_seen.as_ref() == Some(&content) {
            return None;
        }
        self.last_seen = Some(content.clone());
        if self.last_hash == Some(hash(&content)) {
            return None;
        }
        if matches!(&content, ClipboardContent::Text(t) if t.len() > MAX_CLIPBOARD)
            || matches!(&content, ClipboardContent::Image { png, .. } if png.len() > MAX_CLIPBOARD_IMAGE)
        {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
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
    };
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
#[derive(Debug, Default)]
pub struct FakeBackend {
    content: std::cell::RefCell<Option<ClipboardContent>>,
}
impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        *self.content.get_mut() = Some(ClipboardContent::Text(text.into()));
    }
}
impl ClipboardBackend for FakeBackend {
    fn read(&self) -> Option<ClipboardContent> {
        self.content.borrow().clone()
    }
    fn write(&self, content: &ClipboardContent) {
        *self.content.borrow_mut() = Some(content.clone());
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
}
