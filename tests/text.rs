use std::io::ErrorKind;
use std::path::PathBuf;

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::ip_address::IpAddress;
use builder::server::Server;
use builder::text::{TextFile, TextType};

// A unique path in the temp dir so parallel tests don't collide
fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("builder_text_test_{}_{}", std::process::id(), name));
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn text_type_try_from_extension() {
    assert_eq!(TextType::try_from("html").ok(), Some(TextType::Html));
    assert_eq!(TextType::try_from("HTML").ok(), Some(TextType::Html));
    assert_eq!(TextType::try_from("htm").ok(), Some(TextType::Html));
    assert_eq!(TextType::try_from("css").ok(), Some(TextType::Css));
    assert_eq!(TextType::try_from("js").ok(), Some(TextType::Javascript));
    assert_eq!(TextType::try_from("mjs").ok(), Some(TextType::Javascript));
    assert_eq!(TextType::try_from("json").ok(), Some(TextType::Json));
    assert_eq!(TextType::try_from("xml").ok(), Some(TextType::Xml));
    assert_eq!(TextType::try_from("csv").ok(), Some(TextType::Csv));
    assert_eq!(TextType::try_from("md").ok(), Some(TextType::Markdown));
    assert_eq!(TextType::try_from("markdown").ok(), Some(TextType::Markdown));
    assert_eq!(TextType::try_from("txt").ok(), Some(TextType::Plain));
    assert_eq!(TextType::try_from("png").ok(), None);
    assert_eq!(TextType::try_from("").ok(), None);
}

#[test]
fn text_try_from_file_css() {
    let css = b"body { color: red; }";
    let path = temp_file("a.css", css);
    let file = TextFile::try_from(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(file.text_type(), TextType::Css);
    assert_eq!(file.content_type(), "text/css; charset=utf-8");
    assert_eq!(file.into_bytes(), css);
}

#[test]
fn text_try_from_file_javascript() {
    let js = b"console.log(\"hi\");";
    let path = temp_file("a.js", js);
    let file = TextFile::try_from(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(file.text_type(), TextType::Javascript);
    assert_eq!(file.content_type(), "text/javascript; charset=utf-8");
    assert_eq!(file.into_bytes(), js);
}

#[test]
fn text_try_from_file_unsupported_extension() {
    let err = TextFile::try_from("not_text.png").err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Unsupported);

    let err = TextFile::try_from("no_extension").err().unwrap();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[test]
fn text_try_from_file_missing() {
    let err = TextFile::try_from("definitely_missing_builder_page.html").err().unwrap();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn text_served_by_endpoint() {
    const CSS: &str = "h1 { margin: 0; }";
    let mut s = Server::new(());
    s.add_endpoint(Method::Get, "/style.css", |_request: &HttpRequest, _state: &()| -> HttpResponse {
        let file = TextFile::new(CSS, TextType::Css);
        HttpResponseBuilder::from_text_file(Status::Ok, file).build()
    });

    let request = HttpRequest::new("GET /style.css HTTP/1.1\r\nHost: www.example.com\r\n\r\n").unwrap();
    let response = s.invoke(&request, &IpAddress::from([127, 0, 0, 1]));

    assert_eq!(response.status_code(), 200);
    assert_eq!(response.header("Content-Type"), Some("text/css; charset=utf-8"));
    assert_eq!(response.header("Content-Length"), Some(CSS.len().to_string().as_str()));
    assert_eq!(response.body(), CSS.as_bytes());
}

#[test]
fn text_result_ok_and_error() {
    let html = b"<p>hi</p>";
    let path = temp_file("a.html", html);
    let ok = HttpResponseBuilder::from_text_file_result(Status::Ok, TextFile::try_from(&path)).build();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(ok.status_code(), 200);
    assert_eq!(ok.header("Content-Type"), Some("text/html; charset=utf-8"));
    assert_eq!(ok.body(), html);

    let missing = TextFile::try_from("definitely_missing_builder_page.html");
    let err = HttpResponseBuilder::from_text_file_result(Status::Ok, missing).build();
    assert_eq!(err.status_code(), 500);
    assert!(err.body().is_empty());
}
