use std::collections::HashMap;

use crate::html::Html;
use crate::image::Image;

pub use crate::data::http_response_status_codes::Status;

fn reason_phrase(status: Status) -> &'static str {
    use Status as S;

    match status {
        S::Continue => "Continue",
        S::SwitchingProtocols => "Switching Protocols",
        S::EarlyHints => "Early Hints",

        S::Ok => "OK",
        S::Created => "Created",
        S::Accepted => "Accepted",
        S::NonAuthoritativeInformation => "Non-Authoritative Information",
        S::NoContent => "No Content",
        S::ResetContent => "Reset Content",
        S::PartialContent => "Partial Content",
        S::MultiStatus => "Multi-Status",
        S::AlreadyReported => "Already Reported",
        S::ImUsed => "IM Used",

        S::MultipleChoices => "Multiple Choices",
        S::MovedPermanently => "Moved Permanently",
        S::Found => "Found",
        S::SeeOther => "See Other",
        S::NotModified => "Not Modified",
        S::TemporaryRedirect => "Temporary Redirect",
        S::PermanentRedirect => "Permanent Redirect",

        S::BadRequest => "Bad Request",
        S::Unauthorized => "Unauthorized",
        S::PaymentRequired => "Payment Required",
        S::Forbidden => "Forbidden",
        S::NotFound => "Not Found",
        S::MethodNotAllowed => "Method Not Allowed",
        S::NotAcceptable => "Not Acceptable",
        S::ProxyAuthenticationRequired => "Proxy Authentication Required",
        S::RequestTimeout => "Request Timeout",
        S::Conflict => "Conflict",
        S::Gone => "Gone",
        S::LengthRequired => "Length Required",
        S::PreconditionFailed => "Precondition Failed",
        S::ContentTooLarge => "Content Too Large",
        S::UriTooLong => "URI Too Long",
        S::UnsupportedMediaType => "Unsupported Media Type",
        S::RangeNotSatisfiable => "Range Not Satisfiable",
        S::ExpectationFailed => "Expectation Failed",
        S::ImATeapot => "I'm a teapot",
        S::MisdirectedRequest => "Misdirected Request",
        S::UnprocessableContent => "Unprocessable Content",
        S::Locked => "Locked",
        S::FailedDependency => "Failed Dependency",
        S::TooEarly => "Too Early",
        S::UpgradeRequired => "Upgrade Required",
        S::TooManyRequests => "Too Many Requests",
        S::RequestHeaderFieldsTooLarge => "Request Header Fields Too Large",
        S::UnavailableForLegalReasons => "Unavailable For Legal Reasons",

        S::InternalServerError => "Internal Server Error",
        S::NotImplemented => "Not Implemented",
        S::BadGateway => "Bad Gateway",
        S::ServiceUnavailable => "Service Unavailable",
        S::GatewayTimeout => "Gateway Timeout",
        S::HttpVersionNotSupported => "HTTP Version Not Supported",
        S::VariantAlsoNegotiates => "Variant Also Negotiates",
        S::InsufficientStorage => "Insufficient Storage",
        S::LoopDetected => "Loop Detected",
        S::NotExtended => "Not Extended",
        S::NetworkAuthenticationRequired => "Network Authentication Required",

        S::NoStatus => panic!("HttpResponse has no valid status to format"),
    }
}

// 1xx, 204, and 304 responses must not carry a body or Content-Length
fn allows_body(status: Status) -> bool {
    let code = status as u16;
    code >= 200 && status != Status::NoContent && status != Status::NotModified
}


// Separates the head (status line and headers) from the body
const HEAD_END: &[u8] = b"\r\n\r\n";

// Index just past the blank line that ends the head
fn head_len(bytes: &[u8]) -> usize {
    bytes
        .windows(HEAD_END.len())
        .position(|w| w == HEAD_END)
        .map_or(bytes.len(), |i| i + HEAD_END.len())
}

// Collects the pieces of a response, then builds it into the bytes sent on the wire
pub struct HttpResponseBuilder<'a> {
    status: Status,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    version: &'a str,
}

impl<'a> HttpResponseBuilder<'a> {
    // A response with no body or headers
    pub fn new(status: Status, version: &'a str) -> Self {
        HttpResponseBuilder {
            status,
            headers: HashMap::new(),
            body: Vec::new(),
            version,
        }
    }

    fn with_content(status: Status, body: Vec<u8>, content_type: &str, version: &'a str) -> Self {
        let mut builder = Self::new(status, version);
        builder.body = body;
        builder.headers.insert("Content-Type".to_owned(), content_type.to_owned());
        builder
    }

    pub fn from_text(status: Status, text: impl Into<String>, version: &'a str) -> Self {
        Self::with_content(status, text.into().into_bytes(), "text/plain; charset=utf-8", version)
    }

    pub fn from_html(status: Status, html: Html, version: &'a str) -> Self {
        Self::with_content(status, html.into_string().into_bytes(), "text/html; charset=utf-8", version)
    }

    pub fn from_image(_status: Status, _image: Image, _version: &'a str) -> Self {
        todo!("Image is not defined yet")
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    pub fn headers_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.headers
    }

    pub fn body(&self) -> &Vec<u8> {
        &self.body
    }

    pub fn body_mut(&mut self) -> &mut Vec<u8> {
        &mut self.body
    }

    // Formats the full HTTP response (status line, headers, blank line, body) ready to send
    // Adds a Content-Length header if one wasn't set (and the status allows a body)
    // Consumes self so the head is written into the body's buffer instead of a new allocation
    pub fn build(self) -> HttpResponse {
        let has_body = allows_body(self.status);

        // Status line: VERSION SP CODE SP REASON
        let mut head = format!("{} {} {}\r\n", self.version, self.status as u16, reason_phrase(self.status));

        // Headers: KEY: VALUE
        let mut has_content_length = false;
        for (key, value) in &self.headers {
            // Header names are case-insensitive
            if key.eq_ignore_ascii_case("Content-Length") {
                if !has_body {
                    continue;
                }
                has_content_length = true;
            }

            head.push_str(key);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }

        if has_body && !has_content_length {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }

        // Blank line separates headers from the body
        head.push_str("\r\n");

        let mut response = self.body;
        if !has_body {
            response.clear();
        }
        // Shifts the body back in place and writes the head in front of it
        response.splice(0..0, head.into_bytes());
        HttpResponse(response)
    }
}

// A fully built HTTP response, as the bytes sent on the wire
// Only HttpResponseBuilder::build creates one, so the bytes always hold a status line, headers, and a blank line
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse(Vec<u8>);

impl HttpResponse {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    // The numeric code from the status line: VERSION SP CODE SP REASON
    pub fn status_code(&self) -> u16 {
        self.0
            .iter()
            .position(|&b| b == b' ')
            .and_then(|i| self.0.get(i + 1..i + 4))
            .and_then(|code| std::str::from_utf8(code).ok())
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    // The status line and headers, including the blank line that ends them
    pub fn head(&self) -> &[u8] {
        &self.0[..head_len(&self.0)]
    }

    pub fn body(&self) -> &[u8] {
        &self.0[head_len(&self.0)..]
    }

    // The value of the first header named name (case-insensitive)
    pub fn header(&self, name: &str) -> Option<&str> {
        let head = std::str::from_utf8(self.head()).ok()?;
        head.split("\r\n")
            .skip(1) // Status line
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    }

    // Drops the body but keeps the head, so a HEAD response keeps the GET's Content-Length
    pub fn strip_body(&mut self) {
        self.0.truncate(head_len(&self.0));
    }
}
