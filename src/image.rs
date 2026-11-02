use std::io;
use std::path::Path;

// The image formats an Image can be served as
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
    Svg,
    Ico,
    Bmp,
}

impl ImageType {
    // Matches a file extension (without the dot), ignoring case
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.to_ascii_lowercase();
        match ext.as_str() {
            "png" => Some(ImageType::Png),
            "jpg" | "jpeg" => Some(ImageType::Jpeg),
            "gif" => Some(ImageType::Gif),
            "webp" => Some(ImageType::Webp),
            "avif" => Some(ImageType::Avif),
            "svg" => Some(ImageType::Svg),
            "ico" => Some(ImageType::Ico),
            "bmp" => Some(ImageType::Bmp),
            _ => None,
        }
    }

    // The value for the Content-Type header
    pub fn mime(self) -> &'static str {
        match self {
            ImageType::Png => "image/png",
            ImageType::Jpeg => "image/jpeg",
            ImageType::Gif => "image/gif",
            ImageType::Webp => "image/webp",
            ImageType::Avif => "image/avif",
            ImageType::Svg => "image/svg+xml",
            ImageType::Ico => "image/x-icon",
            ImageType::Bmp => "image/bmp",
        }
    }
}

// An image to send as a response body
pub struct Image {
    data: Vec<u8>,
    image_type: ImageType,
}

impl Image {
    pub fn new(data: impl Into<Vec<u8>>, image_type: ImageType) -> Self {
        Image {
            data: data.into(),
            image_type,
        }
    }

    // The type comes from the file extension
    // Fails with ErrorKind::Unsupported for an unknown extension, before the file is read
    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let image_type = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(ImageType::from_extension)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported image type: {}", path.display()),
                )
            })?;

        let data = std::fs::read(path)?;
        Ok(Image { data, image_type })
    }

    pub fn image_type(&self) -> ImageType {
        self.image_type
    }

    pub fn content_type(&self) -> &'static str {
        self.image_type.mime()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}
