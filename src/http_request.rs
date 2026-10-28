use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    // Number of elements in enum
    pub const COUNT: usize = 5;
}

static METHOD_NAMES: [(&str, Method); Method::COUNT] = [
    ("GET", Method::Get),
    ("POST", Method::Post),
    ("PUT", Method::Put),
    ("PATCH", Method::Patch),
    ("DELETE", Method::Delete),
];

#[derive(Debug)]
pub enum ParseError {
    MissingLineEnding,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MissingLineEnding => write!(f, "Malformed HTTP request: missing line ending"),
        }
    }
}

impl std::error::Error for ParseError {}

// Returns None if it isn't recognized
fn parse_method(method: &str) -> Option<Method> {
    METHOD_NAMES
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, method)| *method)
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t')
}

// Pops the next line off the front of request (without its line ending)
// Accepts both CRLF and bare LF line endings
fn next_line<'a>(request: &mut &'a str) -> Result<&'a str, ParseError> {
    let newline = request.find('\n').ok_or(ParseError::MissingLineEnding)?;

    let line = &request[..newline];
    *request = &request[newline + 1..];

    Ok(line.strip_suffix('\r').unwrap_or(line))
}

// These requests borrow the string that created them
// They therefore cannot outlive the string they were created from
pub struct HttpRequest<'a> {
    method: Option<Method>,
    path: &'a str,
    headers: HashMap<&'a str, &'a str>,
    content: &'a str,
    version: &'a str,
    parsing_error: bool,
}

impl<'a> HttpRequest<'a> {
    // Breaks down the full request into the method, path, headers, content, and version
    pub fn new(mut request: &'a str) -> Result<Self, ParseError> {
        let mut parsed = HttpRequest {
            method: None,
            path: "",
            headers: HashMap::new(),
            content: "",
            version: "",
            parsing_error: false,
        };

        // Request line: METHOD SP PATH SP VERSION
        let request_line = next_line(&mut request)?;

        let parts = request_line
            .split_once(' ')
            .and_then(|(method, rest)| rest.split_once(' ').map(|(path, version)| (method, path, version)));
        let Some((method, path, version)) = parts else {
            parsed.parsing_error = true;
            return Ok(parsed);
        };

        parsed.method = parse_method(method);
        parsed.path = path;
        parsed.version = version;

        // Headers: KEY: VALUE, terminated by an empty line
        loop {
            let line = next_line(&mut request)?;
            if line.is_empty() {
                break;
            }

            match line.split_once(':') {
                Some((key, value)) if !key.is_empty() => {
                    parsed.headers.insert(key, trim(value));
                }
                _ => {
                    parsed.parsing_error = true;
                    return Ok(parsed);
                }
            }
        }

        // Everything after the blank line is the body
        parsed.content = request;

        Ok(parsed)
    }


    pub fn header_value(&self, key: &str) -> Option<&'a str> {
        self.headers.get(key).copied()
    }

    // None if the method wasn't recognized
    pub fn method(&self) -> Option<Method> {
        self.method
    }

    pub fn path(&self) -> &'a str {
        self.path
    }

    pub fn content(&self) -> &'a str {
        self.content
    }

    pub fn version(&self) -> &'a str {
        self.version
    }

    pub fn parsing_error(&self) -> bool {
        self.parsing_error
    }
}
