// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::http_request::{to_text, HttpRequest, Method, ParseError};
use crate::http_response::{HttpResponse, HttpResponseBuilder, Status, VERSIONS_SUPPORTED};
use crate::ip_address::IpAddress;
use crate::tcp_listener::TcpListener;

// The response may borrow from the state, so a handler can return a response cached there without copying it
pub type RequestHandler<S> = for <'r, 'a, 's> fn(&'r HttpRequest<'a>, &'s S) -> HttpResponse<'s>;

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

// The segments of a path between its slashes, one per level of the lookup table
// A slash that ends the path stays on the last segment, so "/about/" is a different path from "/about"
fn segments(path: &str) -> impl Iterator<Item = &str> {
    let mut rest = path.strip_prefix('/').unwrap_or(path);
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (segment, after) = match rest.find('/') {
            Some(slash) if slash != rest.len() - 1 => (&rest[..slash], &rest[slash + 1..]),
            _ => (rest, ""),
        };
        rest = after;
        Some(segment)
    })
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
        // Traverse one step down at a time
        let mut current_state = &mut self.root_lookup;
        for token in segments(uri) {
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
        ) -> HttpResponse<'_> {
        // The parser only accepts well formed versions, so anything else here is just unsupported
        if !VERSIONS_SUPPORTED.contains(&request.version()) {
            log_error!("505 HTTP Version Not Supported: {:?} for {}", request.version(), request.path());
            return HttpResponseBuilder::new(Status::HttpVersionNotSupported).build();
        }

        let Some(method) = request.method() else {
            log_error!("501 Not Implemented: unrecognized method for {}", request.path());
            return HttpResponseBuilder::new(Status::NotImplemented).build();
        };

        // RFC 9112 6.1: only the chunked transfer coding is understood
        if request.header_value("transfer-encoding").is_some_and(|coding| !coding.eq_ignore_ascii_case("chunked")) {
            log_error!("501 Not Implemented: unsupported transfer coding for {}", request.path());
            return HttpResponseBuilder::new(Status::NotImplemented).build();
        }

        // Forward the request to the path that will handle it
        // Traverse the path
        let path = request.path();
        let found = segments(path).try_fold(&self.root_lookup, |level, token| level.next.get(token));

        // Not found if the path is not implemented, or is an intermediate path with no methods defined
        let Some(path_state) = found.filter(|level| level.handlers.iter().any(Option::is_some)) else {
            log_error!("404 Not Found: {:?} {}", method, path);
            return HttpResponseBuilder::new(Status::NotFound).build();
        };

        // Make sure that the method is actually implemented
        // RFC 9110 9.3.2: HEAD falls back to the GET handler, without the content
        let handler = path_state.handlers[method as usize]
            .or_else(|| path_state.handlers[Method::Get as usize].filter(|_| method == Method::Head));
        let Some(handler) = handler else {
            log_error!("405 Method Not Allowed: {:?} {}", method, path);
            // RFC 9110 15.5.6: a 405 must list the methods that are allowed
            let mut response = HttpResponseBuilder::new(Status::MethodNotAllowed);
            response.headers_mut().insert("Allow".to_owned(), allowed_methods(&path_state.handlers));
            return response.build();
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
        // Only the head is checked as UTF-8 here, and the body only if the handler reads it, since validating a
        // 4kb body cost more than routing the request
        let (head, rest) = match HttpRequest::split_head(request) {
            Ok(parts) => parts,
            Err(e) => return self.respond(Err(e), &addr),
        };
        // A head that is not UTF-8 has its invalid sequences replaced, and the parser judges what is left
        // Only the head: the body's lengths count the bytes as they were sent, so it must stay as it is
        let head = to_text(head);
        self.respond(HttpRequest::from_parts(&head, rest), &addr)
    }

    fn respond(&self, parsed: Result<HttpRequest<'_>, ParseError>, addr: &IpAddress) -> Cow<'_, [u8]> {
        match parsed {
            // A handler returning a response cached in the state comes back as Cow::Borrowed
            Ok(request) => self.invoke(&request, addr).into_bytes(),
            Err(e) => {
                log_error!("400 Bad Request: {}", e);
                HttpResponseBuilder::new(Status::BadRequest).build().into_bytes()
            }
        }
    }

    // The state is shared by the threads serving each connection, so it must be Sync
    pub fn run(&self, port: i32)
    where
        S: Sync,
    {
        let mut listener = TcpListener::new(self, Self::handle_raw);
        listener.bind(port);
        listener.run();
    }
}

impl Default for Server<()> {
    // Call self.new with an empty state
    fn default() -> Self {
        Self::new(())
    }
}