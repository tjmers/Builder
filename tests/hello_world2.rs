use std::borrow::Cow;

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::ip_address::IpAddress;
use builder::server::Server;
use builder::text::{TextFile, TextType};

// Host an endpoint on /index that returns H1 with Hello, World

const PAGE: &str = "<h1>Hello, World</h1>";

struct AppState {
    response: HttpResponse<'static>,
}

#[test]
fn testcached() {
    // Create the only response beforehand and store it in the app state so that all responses are cached
    let response = HttpResponseBuilder::from_text_file(Status::Ok, TextFile::new(PAGE, TextType::Html), "HTTP/1.1").build();
    let mut s = Server::new(AppState { response });

    s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, state: &AppState| -> HttpResponse {
        state.response.borrowed()
    });

    let get = HttpRequest::new("GET /index HTTP/1.1\r\nHost: www.example.com\r\n\r\n").unwrap();
    let head = HttpRequest::new("HEAD /index HTTP/1.1\r\nHost: www.example.com\r\n\r\n").unwrap();
    let addr = IpAddress::from([127, 0, 0, 1]);

    // Both GETs return the cached bytes, borrowed rather than copied
    let a = s.invoke(&get, &addr);
    let b = s.invoke(&get, &addr);
    assert_eq!(a.status_code(), 200);
    assert_eq!(a.header("Content-Type"), Some("text/html; charset=utf-8"));
    assert_eq!(a.body(), PAGE.as_bytes());
    assert_eq!(a.as_bytes().as_ptr(), b.as_bytes().as_ptr());
    assert!(matches!(a.into_bytes(), Cow::Borrowed(_)));

    // HEAD falls back to the cached GET: same Content-Length, no body, and still borrowed
    let h = s.invoke(&head, &addr);
    assert_eq!(h.status_code(), 200);
    assert_eq!(h.header("Content-Length"), Some(PAGE.len().to_string().as_str()));
    assert!(h.body().is_empty());
    assert!(matches!(h.into_bytes(), Cow::Borrowed(_)));
}
