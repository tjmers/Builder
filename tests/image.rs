use std::io::ErrorKind;
use std::path::PathBuf;

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::image::{Image, ImageType};
use builder::ip_address::IpAddress;
use builder::server::Server;

// The 8-byte PNG signature, enough to stand in for an image body
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n";

// A unique path in the temp dir so parallel tests don't collide
fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("builder_image_test_{}_{}", std::process::id(), name));
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn image_type_from_extension() {
    assert_eq!(ImageType::from_extension("png"), Some(ImageType::Png));
    assert_eq!(ImageType::from_extension("PNG"), Some(ImageType::Png));
    assert_eq!(ImageType::from_extension("jpg"), Some(ImageType::Jpeg));
    assert_eq!(ImageType::from_extension("Jpg"), Some(ImageType::Jpeg));
    assert_eq!(ImageType::from_extension("jpeg"), Some(ImageType::Jpeg));
    assert_eq!(ImageType::from_extension("gif"), Some(ImageType::Gif));
    assert_eq!(ImageType::from_extension("webp"), Some(ImageType::Webp));
    assert_eq!(ImageType::from_extension("avif"), Some(ImageType::Avif));
    assert_eq!(ImageType::from_extension("svg"), Some(ImageType::Svg));
    assert_eq!(ImageType::from_extension("ico"), Some(ImageType::Ico));
    assert_eq!(ImageType::from_extension("bmp"), Some(ImageType::Bmp));
    assert_eq!(ImageType::from_extension("txt"), None);
    assert_eq!(ImageType::from_extension(""), None);
}

#[test]
fn image_from_file_png() {
    let path = temp_file("a.png", PNG_BYTES);
    let image = Image::from_file(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(image.image_type(), ImageType::Png);
    assert_eq!(image.content_type(), "image/png");
    assert_eq!(image.into_bytes(), PNG_BYTES);
}

#[test]
fn image_from_file_svg() {
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
    let path = temp_file("a.svg", svg);
    let image = Image::from_file(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(image.content_type(), "image/svg+xml");
    assert_eq!(image.into_bytes(), svg);
}

#[test]
fn image_from_file_unsupported_extension() {
    let err = Image::from_file("not_an_image.txt").err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Unsupported);

    let err = Image::from_file("no_extension").err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[test]
fn image_from_file_missing() {
    let err = Image::from_file("definitely_missing_builder_image.png").err().unwrap();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn image_served_by_endpoint() {
    let mut s = Server::new(());
    s.add_endpoint(Method::Get, "/logo", |request: &HttpRequest, _state: &()| -> HttpResponse {
        let image = Image::new(PNG_BYTES, ImageType::Png);
        HttpResponseBuilder::from_image(Status::Ok, image, request.version()).build()
    });

    let request = HttpRequest::new("GET /logo HTTP/1.1\r\nHost: www.example.com\r\n\r\n").unwrap();
    let response = s.invoke(&request, &IpAddress::from([127, 0, 0, 1]));

    assert_eq!(response.status_code(), 200);
    assert_eq!(response.header("Content-Type"), Some("image/png"));
    assert_eq!(response.header("Content-Length"), Some(PNG_BYTES.len().to_string().as_str()));
    assert_eq!(response.body(), PNG_BYTES);
}

#[test]
fn image_result_error_is_500() {
    let image = Image::from_file("definitely_missing_builder_image.png");
    let response = HttpResponseBuilder::from_image_result(Status::Ok, image, "HTTP/1.1").build();

    assert_eq!(response.status_code(), 500);
    assert!(response.body().is_empty());
}
