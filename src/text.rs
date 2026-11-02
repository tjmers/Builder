use std::io;
use std::path::Path;

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

impl TextType {
    // Matches a file extension (without the dot), ignoring case
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.to_ascii_lowercase();
        match ext.as_str() {
            "html" | "htm" => Some(TextType::Html),
            "css" => Some(TextType::Css),
            "js" | "mjs" => Some(TextType::Javascript),
            "json" => Some(TextType::Json),
            "xml" => Some(TextType::Xml),
            "csv" => Some(TextType::Csv),
            "md" | "markdown" => Some(TextType::Markdown),
            "txt" => Some(TextType::Plain),
            _ => None,
        }
    }

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
    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let text_type = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(TextType::from_extension)
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
