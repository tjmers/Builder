use std::borrow::Cow;
use std::collections::HashMap;

use crate::http_request::{HttpRequest, Method};
use crate::http_response::{HttpResponse, HttpResponseBuilder, Status};
use crate::ip_address::IpAddress;
use crate::tcp_listener::TcpListener;

pub type RequestHandler<S> = for <'r, 'a> fn(&'r HttpRequest<'a>, &S) -> HttpResponse;

const VERSIONS_SUPPORTED: [&str; 2] = ["HTTP/1.0", "HTTP/1.1"];

struct UriLookupTable<S> {
    handlers: [Option<RequestHandler<S>>; Method::COUNT],
    next: HashMap<String, UriLookupTable<S>>,
}

impl<S> Default for UriLookupTable<S> {
    fn default() -> Self {
        Self {
            handlers: [None; Method::COUNT],
            next: HashMap::new(),
        }
    }
}

// Finds the next '/' in s at or after from
fn next_slash(s: &str, from: usize) -> Option<usize> {
    s[from..].find('/').map(|i| i + from)
}

// Determines if any has a valid option
fn any_valid_methods<T, const N: usize>
(arr: &[Option<T>; N]) -> bool {
    arr.iter().any(|opt| opt.is_some())
}

// Comma separated names of the methods with handlers, for the Allow header
fn allowed_methods<S>(handlers: &[Option<RequestHandler<S>>; Method::COUNT]) -> String {
    let has = |method: Method| handlers[method as usize].is_some();
    Method::ALL
        .into_iter()
        .filter(|&method| has(method) || (method == Method::Head && has(Method::Get)))
        .map(Method::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct Server<S = ()> {
    root_lookup: UriLookupTable<S>,
    state: S,
}


impl<S> Server<S> {
    pub fn new(state: S) -> Self {
        Self {
            root_lookup: UriLookupTable::default(),
            state,
        }
    }

    pub fn add_endpoint(
        &mut self,
        method: Method,
        uri: &str,
        callback: RequestHandler<S>
    ) -> bool {
        if uri.is_empty() {
            log_error!("Cannot add an endpoint with an empty uri");
            return false;
        }
        let mut token_start = usize::from(uri.starts_with('/'));
        let mut slash = next_slash(uri, token_start);

        let mut current_state = &mut self.root_lookup;

        while let Some(s) = slash.filter(|&s| s != uri.len() - 1) {
            // Traverse one step down at a time
            let token = &uri[token_start..s];
            current_state = current_state.next.entry(token.to_owned()).or_default();

            token_start = s + 1;
            slash = next_slash(uri, token_start);
        }

        if token_start < uri.len() {
            // Process the final one
            let token = &uri[token_start..];
            current_state = current_state.next.entry(token.to_owned()).or_default();
        }

        let handler = &mut current_state.handlers[method as usize];
        if handler.is_some() {
            // A handler already exists for that type
            log_error!("A handler is already registered for {:?} {}", method, uri);
            return false;
        }

        *handler = Some(callback);

        true
    }

    pub fn invoke<'a>(
        &self,
        request: &HttpRequest<'a>,
        _address: &IpAddress
        ) -> HttpResponse {
        // The parser only accepts well formed versions, so anything else here is just unsupported
        if !VERSIONS_SUPPORTED.contains(&request.version()) {
            log_error!("505 HTTP Version Not Supported: {:?} for {}", request.version(), request.path());
            return HttpResponseBuilder::new(Status::HttpVersionNotSupported, "HTTP/1.1").build();
        }

        let Some(method) = request.method() else {
            log_error!("501 Not Implemented: unrecognized method for {}", request.path());
            return HttpResponseBuilder::new(Status::NotImplemented, request.version()).build();
        };

        // RFC 9112 6.1: only the chunked transfer coding is understood
        if request.header_value("transfer-encoding").is_some_and(|coding| !coding.eq_ignore_ascii_case("chunked")) {
            log_error!("501 Not Implemented: unsupported transfer coding for {}", request.path());
            return HttpResponseBuilder::new(Status::NotImplemented, request.version()).build();
        }

        // Forward the request to the path that will handle it
        // Traverse the path
        let path = request.path();
        let mut path_state = &self.root_lookup;
        let mut token_start = usize::from(path.starts_with('/'));
        let mut slash = next_slash(path, token_start);

        while let Some(s) = slash.filter(|&s| s != path.len() - 1) {
            let token = &path[token_start..s];
            let Some(next_level) = path_state.next.get(token) else {
                // Path is not implemented
                log_error!("404 Not Found: {:?} {}", method, path);
                return HttpResponseBuilder::new(Status::NotFound, request.version()).build();
            };
            path_state = next_level;

            token_start = s + 1;
            slash = next_slash(path, token_start);
        }

        // Process final
        if token_start < path.len() {
            let token = &path[token_start..];
            let Some(next_level) = path_state.next.get(token) else {
                // Path is not implemented
                log_error!("404 Not Found: {:?} {}", method, path);
                return HttpResponseBuilder::new(Status::NotFound, request.version()).build();
            };
            path_state = next_level;
        }

        // Make sure that the method is actually implemented
        // RFC 9110 9.3.2: HEAD falls back to the GET handler, without the content
        let handler = path_state.handlers[method as usize]
            .or_else(|| path_state.handlers[Method::Get as usize].filter(|_| method == Method::Head));
        let Some(handler) = handler else {
            if any_valid_methods(&path_state.handlers) {
                log_error!("405 Method Not Allowed: {:?} {}", method, path);
                // RFC 9110 15.5.6: a 405 must list the methods that are allowed
                let mut response = HttpResponseBuilder::new(Status::MethodNotAllowed, request.version());
                response.headers_mut().insert("Allow".to_owned(), allowed_methods(&path_state.handlers));
                return response.build();
            } else {
                // We return not found if we are at an intermediate path with no methods defined
                log_error!("404 Not Found: {:?} {}", method, path);
                return HttpResponseBuilder::new(Status::NotFound, request.version()).build();
            }
        };

        // Dispatch
        let mut response = handler(request, &self.state);
        if method == Method::Head {
            // The head already has the Content-Length the GET would have sent
            response.strip_body();
        }
        log_info!("{:?} {} -> {}", method, path, response.status_code());
        response
    }

    // The listener's raw request handler: parses the request bytes and returns the response bytes to send
    // A cached response is returned as Cow::Borrowed from self, so the only copy is the one send makes
    fn handle_raw<'s>(&'s self, request: &[u8], addr: IpAddress) -> Cow<'s, [u8]> {
        // Borrows the bytes when they are valid UTF-8, and only copies to replace invalid sequences
        let request = String::from_utf8_lossy(request);
        let request = match HttpRequest::new(&request) {
            Ok(req) => req,
            Err(e) => {
                log_error!("400 Bad Request: {}", e);
                return Cow::Owned(HttpResponseBuilder::new(Status::BadRequest, "HTTP/1.1").build().into_bytes());
            }
        };
        // A cache lookup would go here and return Cow::Borrowed for a hit
        Cow::Owned(self.invoke(&request, &addr).into_bytes())
    }

    pub fn run(&self, port: i32) {
        let mut listener = TcpListener::new(self, Self::handle_raw);
        listener.bind(port);
        listener.run();
    }
}
