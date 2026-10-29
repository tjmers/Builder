// Specification tests for Server::invoke, based on RFC 9110 (HTTP semantics) and RFC 9112 (HTTP/1.1)
// Many of these are expected to fail until invoke and HttpRequest::new implement the full behavior

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, Status};
use builder::ip_address::IpAddress;
use builder::server::{RequestHandler, Server};

fn addr() -> IpAddress {
    IpAddress::from([127, 0, 0, 1])
}

// Every handler's body names the route it serves so tests can tell which one ran
fn test_server() -> Server {
    let mut s = Server::new(());
    s.add_endpoint(Method::Get, "/", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "root", request.version())
    });
    s.add_endpoint(Method::Get, "/about", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "about", request.version())
    });
    // Handlers are fn pointers and cannot capture, so each method gets its own closure
    let multi: [(Method, RequestHandler<()>); 5] = [
        (Method::Get, |request: &HttpRequest, _state: &()| HttpResponse::new(Status::Ok, "GET", request.version())),
        (Method::Post, |request: &HttpRequest, _state: &()| HttpResponse::new(Status::Ok, "POST", request.version())),
        (Method::Put, |request: &HttpRequest, _state: &()| HttpResponse::new(Status::Ok, "PUT", request.version())),
        (Method::Patch, |request: &HttpRequest, _state: &()| HttpResponse::new(Status::Ok, "PATCH", request.version())),
        (Method::Delete, |request: &HttpRequest, _state: &()| HttpResponse::new(Status::Ok, "DELETE", request.version())),
    ];
    for (method, handler) in multi {
        s.add_endpoint(method, "/multi", handler);
    }
    // Leaves /a and /a/b as intermediate nodes without handlers
    s.add_endpoint(Method::Get, "/a/b/c", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "abc", request.version())
    });
    s.add_endpoint(Method::Post, "/echo", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, request.content(), request.version())
    });
    s.add_endpoint(Method::Get, "/host", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, request.header_value("host").unwrap_or("<missing>"), request.version())
    });
    s
}

// A well formed GET request for path
fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n")
}

// For well formed input that HttpRequest::new must accept
fn send<'a>(server: &Server, raw: &'a str) -> HttpResponse<'a> {
    let request = HttpRequest::new(raw).expect("HttpRequest::new failed on a well formed request");
    server.invoke(&request, &addr())
}

fn assert_status(server: &Server, raw: &str, expected: Status) {
    assert_eq!(send(server, raw).status(), expected, "request: {raw:?}");
}

// For malformed input: either HttpRequest::new rejects it, or invoke responds with expected
fn assert_rejected(server: &Server, raw: &str, expected: Status) {
    if let Ok(request) = HttpRequest::new(raw) {
        assert_eq!(server.invoke(&request, &addr()).status(), expected, "request: {raw:?}");
    }
}

// Header names are case-insensitive
fn header<'a>(response: &'a HttpResponse, name: &str) -> Option<&'a str> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// Happy path / dispatch
// ---------------------------------------------------------------------------

#[test]
fn get_root() {
    let raw = get("/");
    let response = send(&test_server(), &raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "root");
}

#[test]
fn get_about() {
    let raw = get("/about");
    let response = send(&test_server(), &raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

#[test]
fn each_method_reaches_its_own_handler() {
    let s = test_server();
    for name in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
        let raw = format!("{name} /multi HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n");
        let response = send(&s, &raw);
        assert_eq!(response.status(), Status::Ok, "method: {name}");
        assert_eq!(response.body(), name);
    }
}

#[test]
fn nested_path_dispatches() {
    let raw = get("/a/b/c");
    let response = send(&test_server(), &raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "abc");
}

#[test]
fn handler_status_passes_through() {
    let mut s = Server::new(());
    s.add_endpoint(Method::Get, "/created", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Created, "made", request.version())
    });
    s.add_endpoint(Method::Get, "/error", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::InternalServerError, "oops", request.version())
    });
    s.add_endpoint(Method::Get, "/empty", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::NoContent, "", request.version())
    });

    let raw = get("/created");
    let created = send(&s, &raw);
    assert_eq!(created.status(), Status::Created);
    assert_eq!(created.body(), "made");

    let raw = get("/error");
    let error = send(&s, &raw);
    assert_eq!(error.status(), Status::InternalServerError);
    assert_eq!(error.body(), "oops");

    assert_eq!(send(&s, &get("/empty")).status(), Status::NoContent);
}

#[test]
fn ipv6_address_is_handled_like_ipv4() {
    let s = test_server();
    let raw = get("/about");
    let request = HttpRequest::new(&raw).unwrap();
    let v6 = IpAddress::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let response = s.invoke(&request, &v6);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

#[test]
fn duplicate_endpoint_keeps_first_handler() {
    let mut s = Server::new(());
    assert!(s.add_endpoint(Method::Get, "/dup", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "first", request.version())
    }));
    assert!(!s.add_endpoint(Method::Get, "/dup", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "second", request.version())
    }));
    assert_eq!(send(&s, &get("/dup")).body(), "first");
}

#[test]
fn endpoint_registered_without_leading_slash() {
    let mut s = Server::new(());
    s.add_endpoint(Method::Get, "about", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "about", request.version())
    });
    let raw = get("/about");
    let response = send(&s, &raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

#[test]
fn bare_lf_line_endings_are_accepted() {
    let response = send(&test_server(), "GET /about HTTP/1.1\nHost: example.com\n\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

// ---------------------------------------------------------------------------
// Empty / truncated input
// ---------------------------------------------------------------------------

#[test]
fn empty_string_is_rejected() {
    assert_rejected(&test_server(), "", Status::BadRequest);
}

#[test]
fn lone_crlf_is_rejected() {
    assert_rejected(&test_server(), "\r\n", Status::BadRequest);
}

#[test]
fn request_line_without_line_ending_is_rejected() {
    assert_rejected(&test_server(), "GET / HTTP/1.1", Status::BadRequest);
}

#[test]
fn headers_without_terminating_blank_line_are_rejected() {
    assert_rejected(&test_server(), "GET / HTTP/1.1\r\nHost: example.com\r\n", Status::BadRequest);
}

#[test]
fn whitespace_only_is_rejected() {
    let s = test_server();
    assert_rejected(&s, "   ", Status::BadRequest);
    assert_rejected(&s, "   \r\n\r\n", Status::BadRequest);
}

// ---------------------------------------------------------------------------
// Request line
// ---------------------------------------------------------------------------

#[test]
fn request_line_missing_parts_is_rejected() {
    let s = test_server();
    assert_rejected(&s, "GET\r\nHost: example.com\r\n\r\n", Status::BadRequest);
    assert_rejected(&s, "GET /\r\nHost: example.com\r\n\r\n", Status::BadRequest);
}

#[test]
fn request_line_with_extra_token_is_rejected() {
    assert_rejected(&test_server(), "GET / HTTP/1.1 x\r\nHost: example.com\r\n\r\n", Status::BadRequest);
}

#[test]
fn request_line_with_double_space_is_rejected() {
    assert_rejected(&test_server(), "GET  / HTTP/1.1\r\nHost: example.com\r\n\r\n", Status::BadRequest);
}

#[test]
fn request_line_with_tabs_is_rejected() {
    assert_rejected(&test_server(), "GET\t/\tHTTP/1.1\r\nHost: example.com\r\n\r\n", Status::BadRequest);
}

#[test]
fn empty_request_target_is_rejected() {
    assert_rejected(&test_server(), "GET  HTTP/1.1\r\nHost: example.com\r\n\r\n", Status::BadRequest);
}

#[test]
fn request_target_without_leading_slash_is_rejected() {
    assert_rejected(&test_server(), &get("about"), Status::BadRequest);
}

// RFC 9112 2.2: a server SHOULD ignore at least one empty line before the request line
#[test]
fn leading_empty_line_is_ignored() {
    let response = send(&test_server(), "\r\nGET /about HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

#[test]
fn non_ascii_request_target_is_rejected() {
    assert_rejected(&test_server(), &get("/café"), Status::BadRequest);
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

// Method names are case-sensitive
#[test]
fn lowercase_method_is_not_implemented() {
    assert_status(&test_server(), "get /about HTTP/1.1\r\nHost: example.com\r\n\r\n", Status::NotImplemented);
}

#[test]
fn unknown_method_is_not_implemented() {
    assert_status(&test_server(), "FOO /about HTTP/1.1\r\nHost: example.com\r\n\r\n", Status::NotImplemented);
}

#[test]
fn unsupported_standard_methods_are_not_implemented() {
    let s = test_server();
    for method in ["OPTIONS", "CONNECT", "TRACE"] {
        let raw = format!("{method} /about HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_status(&s, &raw, Status::NotImplemented);
    }
}

#[test]
fn unknown_method_is_checked_before_path() {
    assert_status(&test_server(), "FOO /missing HTTP/1.1\r\nHost: example.com\r\n\r\n", Status::NotImplemented);
}

#[test]
fn known_method_without_handler_is_not_allowed() {
    let raw = "POST /about HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n";
    assert_status(&test_server(), raw, Status::MethodNotAllowed);
}

// RFC 9110 15.5.6: a 405 response MUST include an Allow header
#[test]
fn method_not_allowed_lists_allowed_methods() {
    let raw = "POST /about HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n";
    let response = send(&test_server(), raw);
    assert_eq!(response.status(), Status::MethodNotAllowed);

    let allow = header(&response, "Allow").expect("405 response is missing the Allow header");
    let methods: Vec<&str> = allow.split(',').map(str::trim).collect();
    assert!(methods.contains(&"GET"), "Allow: {allow}");
    assert!(!methods.contains(&"POST"), "Allow: {allow}");
}

// RFC 9110 9.3.2: HEAD is identical to GET except the server MUST NOT send content
#[test]
fn head_on_get_route_has_no_body() {
    let response = send(&test_server(), "HEAD /about HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "");
}

#[test]
fn head_on_unknown_path_is_not_found() {
    assert_status(&test_server(), "HEAD /missing HTTP/1.1\r\nHost: example.com\r\n\r\n", Status::NotFound);
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

#[test]
fn unsupported_version_is_rejected_with_505() {
    let s = test_server();
    for version in ["HTTP/2.0", "HTTP/3.0", "HTTP/9.9"] {
        let raw = format!("GET /about {version}\r\nHost: example.com\r\n\r\n");
        assert_status(&s, &raw, Status::HttpVersionNotSupported);
    }
}

#[test]
fn malformed_version_is_rejected() {
    let s = test_server();
    for version in ["http/1.1", "HTTP/1.1x", "HTTP/", "HTTP/1", "1.1"] {
        let raw = format!("GET /about {version}\r\nHost: example.com\r\n\r\n");
        assert_rejected(&s, &raw, Status::BadRequest);
    }
}

#[test]
fn http_1_0_is_accepted() {
    let response = send(&test_server(), "GET /about HTTP/1.0\r\nHost: example.com\r\n\r\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

// RFC 9112 3.2: a server MUST respond 400 to an HTTP/1.1 request without exactly one Host header
#[test]
fn missing_host_is_rejected() {
    assert_rejected(&test_server(), "GET /about HTTP/1.1\r\n\r\n", Status::BadRequest);
}

#[test]
fn duplicate_host_is_rejected() {
    let raw = "GET /about HTTP/1.1\r\nHost: example.com\r\nHost: example.org\r\n\r\n";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

#[test]
fn empty_host_value_is_allowed() {
    assert_status(&test_server(), "GET /about HTTP/1.1\r\nHost:\r\n\r\n", Status::Ok);
}

// RFC 9112 5.1: whitespace between the field name and colon MUST be rejected with 400
#[test]
fn whitespace_before_colon_is_rejected() {
    assert_rejected(&test_server(), "GET /about HTTP/1.1\r\nHost : example.com\r\n\r\n", Status::BadRequest);
}

// RFC 9112 5.2: obsolete line folding
#[test]
fn obs_fold_is_rejected() {
    let raw = "GET /about HTTP/1.1\r\nHost: example.com\r\nX-Long: part1\r\n part2:more\r\n\r\n";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

#[test]
fn header_without_colon_is_rejected() {
    let raw = "GET /about HTTP/1.1\r\nHost: example.com\r\nNoColonHere\r\n\r\n";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

#[test]
fn empty_header_name_is_rejected() {
    let raw = "GET /about HTTP/1.1\r\nHost: example.com\r\n: value\r\n\r\n";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

#[test]
fn bare_cr_in_header_value_is_rejected() {
    let raw = "GET /about HTTP/1.1\r\nHost: example.com\r\nX-Test: a\rb\r\n\r\n";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

#[test]
fn header_names_are_case_insensitive() {
    let response = send(&test_server(), "GET /host HTTP/1.1\r\nHOST: example.com\r\n\r\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "example.com");
}

#[test]
fn header_value_whitespace_is_trimmed() {
    let response = send(&test_server(), "GET /host HTTP/1.1\r\nhost: \t example.com \t\r\n\r\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "example.com");
}

// ---------------------------------------------------------------------------
// Routing / paths
// ---------------------------------------------------------------------------

#[test]
fn unknown_path_is_not_found() {
    assert_status(&test_server(), &get("/missing"), Status::NotFound);
}

#[test]
fn paths_are_case_sensitive() {
    assert_status(&test_server(), &get("/About"), Status::NotFound);
}

#[test]
fn path_deeper_than_registered_is_not_found() {
    assert_status(&test_server(), &get("/a/b/c/d"), Status::NotFound);
}

#[test]
fn double_slash_is_a_distinct_path() {
    assert_status(&test_server(), &get("//about"), Status::NotFound);
}

#[test]
fn trailing_slash_is_a_distinct_path() {
    assert_status(&test_server(), &get("/about/"), Status::NotFound);
}

#[test]
fn intermediate_path_without_handler_is_not_found() {
    let s = test_server();
    assert_status(&s, &get("/a"), Status::NotFound);
    assert_status(&s, &get("/a/b"), Status::NotFound);
}

#[test]
fn empty_server_root_is_not_found() {
    assert_status(&Server::new(()), &get("/"), Status::NotFound);
}

#[test]
fn query_string_is_ignored_for_routing() {
    let s = test_server();

    let raw = get("/about?x=1&y=2");
    let about = send(&s, &raw);
    assert_eq!(about.status(), Status::Ok);
    assert_eq!(about.body(), "about");

    let raw = get("/?");
    let root = send(&s, &raw);
    assert_eq!(root.status(), Status::Ok);
    assert_eq!(root.body(), "root");
}

// RFC 9112 3.2.2: a server MUST accept the absolute-form
#[test]
fn absolute_form_target_is_routed() {
    let raw = get("http://example.com/about");
    let response = send(&test_server(), &raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

// %61 is 'a'
#[test]
fn percent_encoded_path_is_decoded() {
    let raw = get("/%61bout");
    let response = send(&test_server(), &raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "about");
}

#[test]
fn invalid_percent_escape_is_rejected() {
    assert_rejected(&test_server(), &get("/%zz"), Status::BadRequest);
}

// RFC 3986 5.2.4: remove_dot_segments
#[test]
fn dot_segments_are_normalized() {
    let s = test_server();
    for path in ["/x/../about", "/./about"] {
        let raw = get(path);
        let response = send(&s, &raw);
        assert_eq!(response.status(), Status::Ok, "path: {path}");
        assert_eq!(response.body(), "about", "path: {path}");
    }
}

// ---------------------------------------------------------------------------
// Body framing
// ---------------------------------------------------------------------------

#[test]
fn body_is_passed_to_handler() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhello";
    let response = send(&test_server(), raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "hello");
}

#[test]
fn missing_body_is_empty() {
    let response = send(&test_server(), "POST /echo HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "");
}

#[test]
fn body_is_limited_to_content_length() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhelloworld";
    let response = send(&test_server(), raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "hello");
}

#[test]
fn body_shorter_than_content_length_is_rejected() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nContent-Length: 10\r\n\r\nhello";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

#[test]
fn invalid_content_length_is_rejected() {
    let s = test_server();
    for value in ["abc", "-1", "1.5", ""] {
        let raw = format!("POST /echo HTTP/1.1\r\nHost: example.com\r\nContent-Length: {value}\r\n\r\nx");
        assert_rejected(&s, &raw, Status::BadRequest);
    }
}

#[test]
fn conflicting_content_lengths_are_rejected() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nContent-Length: 3\r\n\r\nhello";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

// RFC 9112 6.1: a request with both is a likely smuggling attempt
#[test]
fn transfer_encoding_with_content_length_is_rejected() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    assert_rejected(&test_server(), raw, Status::BadRequest);
}

// RFC 9112 6.1: an unknown transfer coding SHOULD be answered with 501
#[test]
fn unsupported_transfer_encoding_is_not_implemented() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: gzip\r\n\r\nxyz";
    assert_status(&test_server(), raw, Status::NotImplemented);
}

#[test]
fn chunked_body_is_decoded() {
    let raw = "POST /echo HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let response = send(&test_server(), raw);
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(response.body(), "hello");
}
