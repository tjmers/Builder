use std::io;
use std::path::{Path, PathBuf};

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

// Matches a file extension (without the dot), ignoring case
// Fails with ErrorKind::Unsupported for an unknown extension
impl TryFrom<&str> for ImageType {
    type Error = io::Error;

    fn try_from(ext: &str) -> io::Result<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Ok(ImageType::Png),
            "jpg" | "jpeg" => Ok(ImageType::Jpeg),
            "gif" => Ok(ImageType::Gif),
            "webp" => Ok(ImageType::Webp),
            "avif" => Ok(ImageType::Avif),
            "svg" => Ok(ImageType::Svg),
            "ico" => Ok(ImageType::Ico),
            "bmp" => Ok(ImageType::Bmp),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported image extension: {}", ext),
            )),
        }
    }
}

impl ImageType {
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
    pub fn load(path: &Path) -> io::Result<Self> {
        let image_type = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| ImageType::try_from(ext).ok())
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

// Loading an image from a path reads the file
impl TryFrom<&Path> for Image {
    type Error = io::Error;

    fn try_from(path: &Path) -> io::Result<Self> {
        Image::load(path)
    }
}

impl TryFrom<&PathBuf> for Image {
    type Error = io::Error;

    fn try_from(path: &PathBuf) -> io::Result<Self> {
        Image::load(path)
    }
}

impl TryFrom<PathBuf> for Image {
    type Error = io::Error;

    fn try_from(path: PathBuf) -> io::Result<Self> {
        Image::load(&path)
    }
}

impl TryFrom<&str> for Image {
    type Error = io::Error;

    fn try_from(path: &str) -> io::Result<Self> {
        Image::load(Path::new(path))
    }
}
