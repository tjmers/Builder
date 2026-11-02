use std::io;
use std::path::{Path, PathBuf};

// The text formats a TextFile can be served as
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextType {
    Html,
    Css,
    Javascript,
    Json,
    Xml,
    Csv,
    Markdown,
    Plain,
}

// Matches a file extension (without the dot), ignoring case
// Fails with ErrorKind::Unsupported for an unknown extension
impl TryFrom<&str> for TextType {
    type Error = io::Error;

    fn try_from(ext: &str) -> io::Result<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "html" | "htm" => Ok(TextType::Html),
            "css" => Ok(TextType::Css),
            "js" | "mjs" => Ok(TextType::Javascript),
            "json" => Ok(TextType::Json),
            "xml" => Ok(TextType::Xml),
            "csv" => Ok(TextType::Csv),
            "md" | "markdown" => Ok(TextType::Markdown),
            "txt" => Ok(TextType::Plain),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported text file extension: {}", ext),
            )),
        }
    }
}

impl TextType {
    // The value for the Content-Type header
    pub fn mime(self) -> &'static str {
        match self {
            TextType::Html => "text/html; charset=utf-8",
            TextType::Css => "text/css; charset=utf-8",
            TextType::Javascript => "text/javascript; charset=utf-8",
            TextType::Json => "application/json; charset=utf-8",
            TextType::Xml => "application/xml; charset=utf-8",
            TextType::Csv => "text/csv; charset=utf-8",
            TextType::Markdown => "text/markdown; charset=utf-8",
            TextType::Plain => "text/plain; charset=utf-8",
        }
    }
}

// A text document (HTML, CSS, JavaScript, ...) to send as a response body
pub struct TextFile {
    data: Vec<u8>,
    text_type: TextType,
}

impl TextFile {
    pub fn new(data: impl Into<Vec<u8>>, text_type: TextType) -> Self {
        TextFile {
            data: data.into(),
            text_type,
        }
    }

    // The type comes from the file extension
    // Fails with ErrorKind::Unsupported for an unknown extension, before the file is read
    pub fn load(path: &Path) -> io::Result<Self> {
        let text_type = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| TextType::try_from(ext).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported text file type: {}", path.display()),
                )
            })?;

        let data = std::fs::read(path)?;
        Ok(TextFile { data, text_type })
    }

    pub fn text_type(&self) -> TextType {
        self.text_type
    }

    pub fn content_type(&self) -> &'static str {
        self.text_type.mime()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

// Loading a text file from a path reads the file
impl TryFrom<&Path> for TextFile {
    type Error = io::Error;

    fn try_from(path: &Path) -> io::Result<Self> {
        TextFile::load(path)
    }
}

impl TryFrom<&PathBuf> for TextFile {
    type Error = io::Error;

    fn try_from(path: &PathBuf) -> io::Result<Self> {
        TextFile::load(path)
    }
}

impl TryFrom<PathBuf> for TextFile {
    type Error = io::Error;

    fn try_from(path: PathBuf) -> io::Result<Self> {
        TextFile::load(&path)
    }
}

impl TryFrom<&str> for TextFile {
    type Error = io::Error;

    fn try_from(path: &str) -> io::Result<Self> {
        TextFile::load(Path::new(path))
    }
}
