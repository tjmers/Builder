use std::io;
use std::path::Path;

// An HTML document to send as a response body
pub struct Html {
    data: Vec<u8>,
}

impl Html {
    pub fn new(html: impl Into<Vec<u8>>) -> Self {
        Html {
            data: html.into(),
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        std::fs::read(path).map(|data| Html { data })
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}
