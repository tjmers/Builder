use std::borrow::Cow;
use std::fmt;

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

// RFC 9110 5.6.2: token = 1*tchar
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
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
fn next_line<'a>(request: &mut &'a str) -> Result<&'a str, ParseError> {
    let newline = request.find('\n').ok_or(ParseError::MissingLineEnding)?;

    let line = &request[..newline];
    *request = &request[newline + 1..];

    Ok(line.strip_suffix('\r').unwrap_or(line))
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
    let path = match strip_scheme(target) {
        Some(rest) => match rest.find(['/', '?', '#']) {
            Some(i) if rest.as_bytes()[i] == b'/' => &rest[i..],
            _ => "/",
        },
        None if target.starts_with('/') => target,
        None => return Err(ParseError::InvalidRequestTarget),
    };

    // The query plays no part in routing
    let path = path.find(['?', '#']).map_or(path, |i| &path[..i]);

    let path = if path.split('/').any(|segment| segment == "." || segment == "..") {
        Cow::Owned(remove_dot_segments(path))
    } else {
        Cow::Borrowed(path)
    };

    if path.contains('%') {
        Ok(Cow::Owned(percent_decode(&path)?))
    } else {
        Ok(path)
    }
}

// RFC 9112 7.1: chunk = chunk-size [ chunk-ext ] CRLF chunk-data CRLF, ending with a 0 size chunk and trailers
fn decode_chunked(mut body: &str) -> Result<String, ParseError> {
    fn line<'a>(body: &mut &'a str) -> Result<&'a str, ParseError> {
        next_line(body).map_err(|_| ParseError::InvalidChunkedBody)
    }
    let mut decoded = String::new();

    loop {
        // Chunk extensions after ';' are ignored
        let size_line = line(&mut body)?;
        let size = trim(size_line.split(';').next().unwrap_or_default());
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseError::InvalidChunkedBody);
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| ParseError::InvalidChunkedBody)?;
        if size == 0 {
            break;
        }

        let data = body.get(..size).ok_or(ParseError::InvalidChunkedBody)?;
        decoded.push_str(data);
        body = &body[size..];

        if !line(&mut body)?.is_empty() {
            return Err(ParseError::InvalidChunkedBody);
        }
    }

    // Trailer fields are ignored, up to the terminating empty line
    while !line(&mut body)?.is_empty() {}

    Ok(decoded)
}

// RFC 9112 6.3: determines the body from Transfer-Encoding or Content-Length
fn parse_body<'a>(headers: &[(&'a str, &'a str)], body: &'a str) -> Result<Cow<'a, str>, ParseError> {
    let values = |name: &'static str| {
        headers
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    };

    let mut content_length = None;
    for value in values("content-length") {
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseError::InvalidContentLength);
        }
        let length: usize = value.parse().map_err(|_| ParseError::InvalidContentLength)?;
        if content_length.is_some_and(|existing| existing != length) {
            return Err(ParseError::InvalidContentLength);
        }
        content_length = Some(length);
    }

    if let Some(encoding) = values("transfer-encoding").next() {
        // RFC 9112 6.1: both together is a likely smuggling attempt
        if content_length.is_some() {
            return Err(ParseError::InvalidContentLength);
        }
        // Unsupported codings are left for the server to answer with 501
        if !encoding.eq_ignore_ascii_case("chunked") {
            return Ok(Cow::Borrowed(""));
        }
        return decode_chunked(body).map(Cow::Owned);
    }

    match content_length {
        Some(length) if body.len() < length => Err(ParseError::IncompleteBody),
        Some(length) => body.get(..length).map(Cow::Borrowed).ok_or(ParseError::InvalidContentLength),
        None => Ok(Cow::Borrowed("")),
    }
}

// These requests borrow the string that created them
// They therefore cannot outlive the string they were created from
pub struct HttpRequest<'a> {
    method: Option<Method>,
    path: Cow<'a, str>,
    headers: Vec<(&'a str, &'a str)>,
    content: Cow<'a, str>,
    version: &'a str,
}

impl<'a> HttpRequest<'a> {
    // Breaks down the full request into the method, path, headers, content, and version
    // Unrecognized methods, versions, and transfer codings parse successfully so the server can answer them
    pub fn new(mut request: &'a str) -> Result<Self, ParseError> {
        // RFC 9112 2.2: empty lines before the request line are ignored
        let mut request_line = next_line(&mut request)?;
        while request_line.is_empty() {
            request_line = next_line(&mut request)?;
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
        loop {
            let line = next_line(&mut request)?;
            if line.is_empty() {
                break;
            }

            // The name must be a token, which also rejects whitespace before the colon and obs-fold
            let (key, value) = line.split_once(':').ok_or(ParseError::InvalidHeader)?;
            let value = trim(value);
            if !is_token(key) || !is_valid_field_value(value) {
                return Err(ParseError::InvalidHeader);
            }
            headers.push((key, value));
        }

        // RFC 9112 3.2: HTTP/1.1 requires exactly one Host header
        let hosts = headers.iter().filter(|(key, _)| key.eq_ignore_ascii_case("host")).count();
        if hosts > 1 || (hosts == 0 && version == "HTTP/1.1") {
            return Err(ParseError::InvalidHost);
        }

        let content = parse_body(&headers, request)?;

        Ok(HttpRequest {
            method: parse_method(method),
            path,
            headers,
            content,
            version,
        })
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
        &self.content
    }

    pub fn version(&self) -> &'a str {
        self.version
    }
}
