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
pub(crate) fn head_len(request: &[u8]) -> Result<usize, ParseError> {
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

// Why a chunked body could not be read to its end
pub(crate) enum ChunkError {
    // It is fine so far, but more bytes are needed
    Incomplete,
    Malformed,
}

// RFC 9112 7.1: chunk = chunk-size [ chunk-ext ] CRLF chunk-data CRLF, ending with a 0 size chunk and trailers
// Walks a chunked body that may still be arriving, picking up where it left off each time more of it has come in,
// so no byte of a body is read more than once, however small the pieces it arrives in and whether or not they end
// on a line
// Framing a request and decoding it both go through here, so they cannot disagree about where a body ends
#[derive(Default)]
pub(crate) struct ChunkWalker {
    // How much of the body is behind it: whole chunks, the size line of the chunk it is in, and once in_trailers
    // whole trailer lines too
    offset: usize,
    // How many bytes after offset have already been searched for the end of a line that has not all arrived
    scanned: usize,
    // The size of the chunk whose data starts at offset, once its size line is behind it
    chunk: Option<usize>,
    // Whether the 0 size chunk has been passed, leaving only the trailers
    in_trailers: bool,
}

impl ChunkWalker {
    // Takes the line starting at offset, without its line ending, once all of it has arrived
    // Split the same way as next_line_bytes, but a line still arriving is only searched from where the last search ended
    fn line<'a>(&mut self, body: &'a [u8]) -> Result<&'a [u8], ChunkError> {
        let rest = &body[self.offset..];
        let Some(newline) = rest[self.scanned..].iter().position(|&b| b == b'\n') else {
            self.scanned = rest.len();
            return Err(ChunkError::Incomplete);
        };
        let line = &rest[..self.scanned + newline];
        self.offset += line.len() + 1;
        self.scanned = 0;
        Ok(line.strip_suffix(b"\r").unwrap_or(line))
    }

    // Carries on through body, which must be the same body as last time with any new bytes on the end
    // Passes the data of each chunk it gets past to on_chunk, once, and returns how many bytes the whole body takes
    // up, including the framing, once its end has arrived
    pub(crate) fn advance<'a>(&mut self, body: &'a [u8], mut on_chunk: impl FnMut(&'a [u8])) -> Result<usize, ChunkError> {
        while !self.in_trailers {
            let Some(size) = self.chunk else {
                // Chunk extensions after ';' are ignored
                let size_line = self.line(body)?;
                let size = trim_bytes(size_line.split(|&b| b == b';').next().unwrap_or_default());
                if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
                    return Err(ChunkError::Malformed);
                }
                let size = std::str::from_utf8(size)
                    .ok()
                    .and_then(|size| usize::from_str_radix(size, 16).ok())
                    .ok_or(ChunkError::Malformed)?;
                if size == 0 {
                    self.in_trailers = true;
                } else {
                    self.chunk = Some(size);
                }
                continue;
            };

            // The data and the line ending after it, which is looked at directly so that anything else there is
            // refused straight away instead of being searched for a line ending that may never come
            let rest = &body[self.offset..];
            if rest.len() < size {
                return Err(ChunkError::Incomplete);
            }
            let (data, after) = rest.split_at(size);
            let ending = match after {
                [b'\n', ..] => 1,
                [b'\r', b'\n', ..] => 2,
                [] | [b'\r'] => return Err(ChunkError::Incomplete),
                _ => return Err(ChunkError::Malformed),
            };
            on_chunk(data);
            self.offset += size + ending;
            self.chunk = None;
        }

        // Trailer fields are ignored, up to the terminating empty line
        while !self.line(body)?.is_empty() {}
        Ok(self.offset)
    }
}

// Walks a chunked body that has arrived in full
pub(crate) fn walk_chunks<'a>(body: &'a [u8], on_chunk: impl FnMut(&'a [u8])) -> Result<usize, ChunkError> {
    ChunkWalker::default().advance(body, on_chunk)
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, ParseError> {
    let mut decoded = Vec::new();
    walk_chunks(body, |data| decoded.extend_from_slice(data)).map_err(|_| ParseError::InvalidChunkedBody)?;
    Ok(decoded)
}

// Content-Length = 1*DIGIT
fn parse_length(value: &[u8]) -> Option<usize> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

// What a request's headers say about its body
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyKind {
    None,
    // Content-Length bytes
    Length(usize),
    Chunked,
    // A transfer coding other than chunked, which is left for the server to answer with 501
    OtherCoding,
}

// Gathers the headers that decide a request's body as the headers go by
// The listener framing a request and the parser reading it both feed their headers through here, so they cannot
// disagree about what kind of body a request has
#[derive(Default)]
struct BodyHeaders {
    content_length: Option<usize>,
    invalid_length: bool,
    // Whether the first Transfer-Encoding is chunked, and only the first counts
    chunked: Option<bool>,
}

impl BodyHeaders {
    // Takes one header, with the whitespace around its value already trimmed
    fn observe(&mut self, name: &[u8], value: &[u8]) {
        if name.eq_ignore_ascii_case(b"content-length") {
            // RFC 9112 6.3: repeated Content-Length headers must all agree
            match parse_length(value) {
                Some(length) if self.content_length.is_none_or(|existing| existing == length) => self.content_length = Some(length),
                _ => self.invalid_length = true,
            }
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") && self.chunked.is_none() {
            self.chunked = Some(value.eq_ignore_ascii_case(b"chunked"));
        }
    }

    // RFC 9112 6.3: the body comes from Transfer-Encoding or Content-Length
    fn kind(&self) -> Result<BodyKind, ParseError> {
        // RFC 9112 6.1: both together is a likely smuggling attempt
        if self.invalid_length || (self.content_length.is_some() && self.chunked.is_some()) {
            return Err(ParseError::InvalidContentLength);
        }
        Ok(match (self.chunked, self.content_length) {
            (Some(true), _) => BodyKind::Chunked,
            (Some(false), _) => BodyKind::OtherCoding,
            (None, Some(length)) => BodyKind::Length(length),
            (None, None) => BodyKind::None,
        })
    }
}

// Reads just enough of a request's head to frame its body: the length of the head (as head_len) and the kind of
// body its headers call for, which is an error when the parser is going to reject them
// None if the head has not all arrived yet
// Lines are split and headers read the same way as in Head::parse, and anything else the parser objects to is left
// for it to find
pub(crate) fn scan_head(request: &[u8]) -> Option<(usize, Result<BodyKind, ParseError>)> {
    let mut rest = request;
    // Empty lines before the request line, and then the request line itself
    while next_line_bytes(&mut rest).ok()?.is_empty() {}

    let mut body = BodyHeaders::default();
    loop {
        let line = next_line_bytes(&mut rest).ok()?;
        if line.is_empty() {
            break;
        }
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            body.observe(&line[..colon], trim_bytes(&line[colon + 1..]));
        }
    }
    Some((request.len() - rest.len(), body.kind()))
}

// Bytes as text, borrowed when they are valid UTF-8 and with invalid sequences replaced otherwise
pub(crate) fn to_text(bytes: &[u8]) -> Cow<'_, str> {
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
    body: BodyKind,
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
        let mut body = BodyHeaders::default();
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
            } else {
                body.observe(key.as_bytes(), value.as_bytes());
            }
            headers.push((key, value));
        }

        // RFC 9112 3.2: HTTP/1.1 requires exactly one Host header
        if hosts > 1 || (hosts == 0 && version == "HTTP/1.1") {
            return Err(ParseError::InvalidHost);
        }
        let body = body.kind()?;

        Ok(Head {
            method: parse_method(method),
            path,
            headers,
            version,
            body,
        })
    }

    // Takes the body off the front of rest, which is what follows the head
    // Borrowed from rest unless it had to be decoded
    fn body<'b>(&self, rest: &'b [u8]) -> Result<Cow<'b, [u8]>, ParseError> {
        match self.body {
            BodyKind::Chunked => decode_chunked(rest).map(Cow::Owned),
            BodyKind::Length(length) if rest.len() < length => Err(ParseError::IncompleteBody),
            BodyKind::Length(length) => Ok(Cow::Borrowed(&rest[..length])),
            // Unsupported codings are left for the server to answer with 501
            BodyKind::None | BodyKind::OtherCoding => Ok(Cow::Borrowed(b"")),
        }
    }

    fn into_request(self, body: Cow<'a, [u8]>) -> HttpRequest<'a> {
        HttpRequest {
            method: self.method,
            path: self.path,
            headers: self.headers,
            body,
            content: OnceLock::new(),
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
    // The body's bytes, and its text, which is only worked out from the bytes once a handler asks for it
    body: Cow<'a, [u8]>,
    content: OnceLock<Cow<'a, str>>,
    version: &'a str,
}

impl<'a> HttpRequest<'a> {
    // Breaks down the full request into the method, path, headers, content, and version
    // Unrecognized methods, versions, and transfer codings parse successfully so the server can answer them
    pub fn new(mut request: &'a str) -> Result<Self, ParseError> {
        let head = Head::parse(&mut request)?;
        let body = head.body(request.as_bytes())?;
        // A Content-Length that splits a character leaves a body that is not text
        if matches!(body, Cow::Borrowed(_)) && !request.is_char_boundary(body.len()) {
            return Err(ParseError::InvalidContentLength);
        }
        Ok(head.into_request(body))
    }

    // Splits the bytes received into the head and what follows it
    pub(crate) fn split_head(request: &[u8]) -> Result<(&[u8], &[u8]), ParseError> {
        Ok(request.split_at(head_len(request)?))
    }

    // Parses a request from its head and the bytes that followed it, as split by split_head
    // Only the head has to be text, so the body is taken from the bytes exactly as they were sent and is only
    // checked as UTF-8 once a handler asks for its content
    pub(crate) fn from_parts(mut head: &'a str, rest: &'a [u8]) -> Result<Self, ParseError> {
        let parsed = Head::parse(&mut head)?;
        let body = parsed.body(rest)?;
        Ok(parsed.into_request(body))
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

    // The body as text, with any invalid UTF-8 replaced, see body for the bytes as they were sent
    pub fn content(&self) -> &str {
        self.content.get_or_init(|| match &self.body {
            Cow::Borrowed(body) => to_text(body),
            // A decoded body belongs to this request, so the text cannot borrow from it for 'a
            Cow::Owned(body) => Cow::Owned(to_text(body).into_owned()),
        })
    }

    // The body's bytes, with any chunked encoding removed
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn version(&self) -> &'a str {
        self.version
    }
}
