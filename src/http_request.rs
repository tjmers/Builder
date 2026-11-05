use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    // Number of elements in enum
    pub const COUNT: usize = 6;

    pub const ALL: [Method; Method::COUNT] = [
        Method::Get,
        Method::Head,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Delete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

#[derive(Debug)]
pub enum ParseError {
    MissingLineEnding,
    InvalidRequestLine,
    InvalidRequestTarget,
    InvalidVersion,
    InvalidHeader,
    InvalidHost,
    InvalidContentLength,
    InvalidChunkedBody,
    IncompleteBody,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            ParseError::MissingLineEnding => "missing line ending",
            ParseError::InvalidRequestLine => "invalid request line",
            ParseError::InvalidRequestTarget => "invalid request target",
            ParseError::InvalidVersion => "invalid HTTP version",
            ParseError::InvalidHeader => "invalid header field",
            ParseError::InvalidHost => "missing or duplicate Host header",
            ParseError::InvalidContentLength => "invalid Content-Length",
            ParseError::InvalidChunkedBody => "invalid chunked body",
            ParseError::IncompleteBody => "body is shorter than Content-Length",
        };
        write!(f, "Malformed HTTP request: {reason}")
    }
}

impl std::error::Error for ParseError {}

// Returns None if it isn't recognized (method names are case-sensitive)
fn parse_method(method: &str) -> Option<Method> {
    Method::ALL.into_iter().find(|m| m.as_str() == method)
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t')
}

// Like trim, for bytes
fn trim_bytes(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

// RFC 9110 5.6.2: tchar, as a table so checking a token costs one lookup per byte
const TCHAR: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        table[b] = c.is_ascii_alphanumeric()
            || matches!(c, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~');
        b += 1;
    }
    table
};

// RFC 9110 5.6.2: token = 1*tchar
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| TCHAR[b as usize])
}

// RFC 9112 2.3: HTTP-version = "HTTP/" DIGIT "." DIGIT
fn is_valid_version(version: &str) -> bool {
    matches!(version.as_bytes(), [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
        if major.is_ascii_digit() && minor.is_ascii_digit())
}

// Field values may hold visible characters, SP, HTAB, and obs-text, but no other controls
fn is_valid_field_value(value: &str) -> bool {
    value.bytes().all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

// Pops the next line off the front of request (without its line ending)
// Accepts both CRLF and bare LF line endings
// Lines are short, so a plain scan beats str::find, whose searcher costs more to set up than the search takes
fn next_line<'a>(request: &mut &'a str) -> Result<&'a str, ParseError> {
    let newline = request.bytes().position(|b| b == b'\n').ok_or(ParseError::MissingLineEnding)?;

    let line = &request[..newline];
    *request = &request[newline + 1..];

    Ok(line.strip_suffix('\r').unwrap_or(line))
}

// Like next_line, for bytes that may not be valid UTF-8
fn next_line_bytes<'a>(request: &mut &'a [u8]) -> Result<&'a [u8], ParseError> {
    let newline = request.iter().position(|&b| b == b'\n').ok_or(ParseError::MissingLineEnding)?;

    let line = &request[..newline];
    *request = &request[newline + 1..];

    Ok(line.strip_suffix(b"\r").unwrap_or(line))
}

// The length of the head: any empty lines before the request line, the request line, and the headers through the
// empty line that ends them, split into lines the same way as next_line
fn head_len(request: &[u8]) -> Result<usize, ParseError> {
    let mut rest = request;
    while next_line_bytes(&mut rest)?.is_empty() {}
    while !next_line_bytes(&mut rest)?.is_empty() {}
    Ok(request.len() - rest.len())
}

// Returns what follows the scheme of an absolute-form target
fn strip_scheme(target: &str) -> Option<&str> {
    ["http://", "https://"].into_iter().find_map(|scheme| {
        target
            .get(..scheme.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
            .map(|_| &target[scheme.len()..])
    })
}

// RFC 3986 5.2.4: resolves "." and ".." segments of an absolute path
fn remove_dot_segments(path: &str) -> String {
    let mut segments = Vec::new();
    let mut parts = path.split('/').skip(1).peekable();

    while let Some(segment) = parts.next() {
        let last = parts.peek().is_none();
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            _ => {
                segments.push(segment);
                continue;
            }
        }
        // A trailing dot segment still leaves the path ending in '/'
        if last {
            segments.push("");
        }
    }

    let mut normalized = String::with_capacity(path.len());
    for segment in segments {
        normalized.push('/');
        normalized.push_str(segment);
    }
    if normalized.is_empty() {
        normalized.push('/');
    }
    normalized
}

fn percent_decode(path: &str) -> Result<String, ParseError> {
    let hex = |b: Option<&u8>| b.and_then(|&b| (b as char).to_digit(16));

    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (Some(high), Some(low)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) else {
                return Err(ParseError::InvalidRequestTarget);
            };
            decoded.push((high * 16 + low) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }

    String::from_utf8(decoded).map_err(|_| ParseError::InvalidRequestTarget)
}

// Reduces a request target to the decoded, normalized path used for routing
// Only allocates when the path needs normalizing or decoding
fn parse_target(target: &str) -> Result<Cow<'_, str>, ParseError> {
    // Anything outside visible ASCII must be percent-encoded
    if !target.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ParseError::InvalidRequestTarget);
    }

    // RFC 9112 3.2.2: absolute-form, the path starts after the authority
    // (origin-form, the usual case, is checked first since it can never start with a scheme)
    let path = if target.starts_with('/') {
        target
    } else {
        match strip_scheme(target) {
            Some(rest) => match rest.bytes().position(|b| matches!(b, b'/' | b'?' | b'#')) {
                Some(i) if rest.as_bytes()[i] == b'/' => &rest[i..],
                _ => "/",
            },
            None => return Err(ParseError::InvalidRequestTarget),
        }
    };

    // The query plays no part in routing
    let path = path.bytes().position(|b| matches!(b, b'?' | b'#')).map_or(path, |i| &path[..i]);

    // A dot segment always follows a '/', so most paths skip splitting entirely
    let has_dot_segment = path.as_bytes().windows(2).any(|w| w == b"/.")
        && path.split('/').any(|segment| segment == "." || segment == "..");
    let path = if has_dot_segment {
        Cow::Owned(remove_dot_segments(path))
    } else {
        Cow::Borrowed(path)
    };

    if path.bytes().any(|b| b == b'%') {
        Ok(Cow::Owned(percent_decode(&path)?))
    } else {
        Ok(path)
    }
}

// RFC 9112 7.1: chunk = chunk-size [ chunk-ext ] CRLF chunk-data CRLF, ending with a 0 size chunk and trailers
fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, ParseError> {
    fn line<'a>(body: &mut &'a [u8]) -> Result<&'a [u8], ParseError> {
        next_line_bytes(body).map_err(|_| ParseError::InvalidChunkedBody)
    }
    let mut decoded = Vec::new();

    loop {
        // Chunk extensions after ';' are ignored
        let size_line = line(&mut body)?;
        let size = trim_bytes(size_line.split(|&b| b == b';').next().unwrap_or_default());
        if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
            return Err(ParseError::InvalidChunkedBody);
        }
        let size = std::str::from_utf8(size)
            .ok()
            .and_then(|size| usize::from_str_radix(size, 16).ok())
            .ok_or(ParseError::InvalidChunkedBody)?;
        if size == 0 {
            break;
        }

        let data = body.get(..size).ok_or(ParseError::InvalidChunkedBody)?;
        decoded.extend_from_slice(data);
        body = &body[size..];

        if !line(&mut body)?.is_empty() {
            return Err(ParseError::InvalidChunkedBody);
        }
    }

    // Trailer fields are ignored, up to the terminating empty line
    while !line(&mut body)?.is_empty() {}

    Ok(decoded)
}

// Content-Length = 1*DIGIT
fn parse_length(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

// Bytes as text, borrowed when they are valid UTF-8 and with invalid sequences replaced otherwise
fn to_text(bytes: &[u8]) -> Cow<'_, str> {
    // from_utf8 has a fast path for ASCII that from_utf8_lossy lacks
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

// The request line and headers, with what the body depends on gathered while reading the headers
struct Head<'a> {
    method: Option<Method>,
    path: Cow<'a, str>,
    headers: Vec<(&'a str, &'a str)>,
    version: &'a str,
    content_length: Option<usize>,
    transfer_encoding: Option<&'a str>,
}

impl<'a> Head<'a> {
    // Parses the head off the front of request, leaving what follows it
    fn parse(request: &mut &'a str) -> Result<Self, ParseError> {
        // RFC 9112 2.2: empty lines before the request line are ignored
        let mut request_line = next_line(request)?;
        while request_line.is_empty() {
            request_line = next_line(request)?;
        }

        // Request line: METHOD SP TARGET SP VERSION, separated by single spaces
        let mut parts = request_line.split(' ');
        let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(ParseError::InvalidRequestLine);
        };
        if !is_token(method) || target.is_empty() {
            return Err(ParseError::InvalidRequestLine);
        }
        if !is_valid_version(version) {
            return Err(ParseError::InvalidVersion);
        }
        let path = parse_target(target)?;

        // Headers: KEY: VALUE, terminated by an empty line
        let mut headers = Vec::new();
        let mut hosts = 0;
        let mut content_length = None;
        let mut invalid_length = false;
        let mut transfer_encoding = None;
        loop {
            let line = next_line(request)?;
            if line.is_empty() {
                break;
            }

            // The name must be a token, which also rejects whitespace before the colon and obs-fold
            let (key, value) = line.split_once(':').ok_or(ParseError::InvalidHeader)?;
            let value = trim(value);
            if !is_token(key) || !is_valid_field_value(value) {
                return Err(ParseError::InvalidHeader);
            }
            if key.eq_ignore_ascii_case("host") {
                hosts += 1;
            } else if key.eq_ignore_ascii_case("content-length") {
                // RFC 9112 6.3: repeated Content-Length headers must all agree
                match parse_length(value) {
                    Some(length) if content_length.is_none_or(|existing| existing == length) => content_length = Some(length),
                    _ => invalid_length = true,
                }
            } else if key.eq_ignore_ascii_case("transfer-encoding") && transfer_encoding.is_none() {
                transfer_encoding = Some(value);
            }
            headers.push((key, value));
        }

        // RFC 9112 3.2: HTTP/1.1 requires exactly one Host header
        if hosts > 1 || (hosts == 0 && version == "HTTP/1.1") {
            return Err(ParseError::InvalidHost);
        }
        // RFC 9112 6.1: both together is a likely smuggling attempt
        if invalid_length || (content_length.is_some() && transfer_encoding.is_some()) {
            return Err(ParseError::InvalidContentLength);
        }

        Ok(Head {
            method: parse_method(method),
            path,
            headers,
            version,
            content_length,
            transfer_encoding,
        })
    }

    // RFC 9112 6.3: determines the body from Transfer-Encoding or Content-Length
    // Returns the body's raw bytes, or its content when it had to be decoded
    fn body<'b>(&self, rest: &'b [u8]) -> Result<Result<&'b [u8], String>, ParseError> {
        if let Some(encoding) = self.transfer_encoding {
            // Unsupported codings are left for the server to answer with 501
            if !encoding.eq_ignore_ascii_case("chunked") {
                return Ok(Ok(b""));
            }
            return decode_chunked(rest).map(|decoded| Err(to_text(&decoded).into_owned()));
        }
        match self.content_length {
            Some(length) if rest.len() < length => Err(ParseError::IncompleteBody),
            Some(length) => Ok(Ok(&rest[..length])),
            None => Ok(Ok(b"")),
        }
    }

    fn into_request(self, body: &'a [u8], content: OnceLock<Cow<'a, str>>) -> HttpRequest<'a> {
        HttpRequest {
            method: self.method,
            path: self.path,
            headers: self.headers,
            body,
            content,
            version: self.version,
        }
    }
}

// These requests borrow the string that created them
// They therefore cannot outlive the string they were created from
pub struct HttpRequest<'a> {
    method: Option<Method>,
    path: Cow<'a, str>,
    headers: Vec<(&'a str, &'a str)>,
    // The body as received, and its text, which is only worked out from the bytes once a handler asks for it
    body: &'a [u8],
    content: OnceLock<Cow<'a, str>>,
    version: &'a str,
}

impl<'a> HttpRequest<'a> {
    // Breaks down the full request into the method, path, headers, content, and version
    // Unrecognized methods, versions, and transfer codings parse successfully so the server can answer them
    pub fn new(mut request: &'a str) -> Result<Self, ParseError> {
        let head = Head::parse(&mut request)?;
        let content = match head.body(request.as_bytes())? {
            Ok(body) => {
                // A Content-Length that splits a character leaves a body that is not text
                let text = request.get(..body.len()).ok_or(ParseError::InvalidContentLength)?;
                Cow::Borrowed(text)
            }
            Err(decoded) => Cow::Owned(decoded),
        };
        Ok(head.into_request(b"", OnceLock::from(content)))
    }

    // Parses a request straight from the bytes received, where only the head has to be valid UTF-8 up front and
    // the body is only checked once a handler asks for its content
    // None if the head is not valid UTF-8, in which case the caller decides how to handle it
    pub(crate) fn from_bytes(request: &'a [u8]) -> Option<Result<Self, ParseError>> {
        let head_len = match head_len(request) {
            Ok(head_len) => head_len,
            Err(e) => return Some(Err(e)),
        };
        let mut head = std::str::from_utf8(&request[..head_len]).ok()?;
        let parsed = Head::parse(&mut head).and_then(|parsed| {
            let rest = &request[head_len..];
            Ok(match parsed.body(rest)? {
                Ok(body) => parsed.into_request(body, OnceLock::new()),
                Err(decoded) => parsed.into_request(b"", OnceLock::from(Cow::Owned(decoded))),
            })
        });
        Some(parsed)
    }

    // Header names are case-insensitive
    pub fn header_value(&self, key: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| *value)
    }

    // None if the method wasn't recognized
    pub fn method(&self) -> Option<Method> {
        self.method
    }

    // Decoded and normalized, without the query
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn content(&self) -> &str {
        self.content.get_or_init(|| to_text(self.body))
    }

    pub fn version(&self) -> &'a str {
        self.version
    }
}
