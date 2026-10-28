use std::collections::HashMap;

use crate::http_request::{HttpRequest, Method};
use crate::http_response::{HttpResponse, Status};
use crate::ip_address::IpAddress;
use crate::tcp_listener::TcpListener;

pub type RequestHandler = Box<dyn Fn(&HttpRequest<'_>) -> HttpResponse>;

const SUPPORTED_VERSIONS: [&str; 1] = ["HTTP/1.1"];

#[derive(Default)]
struct UriLookupTable {
    handlers: [Option<RequestHandler>; Method::COUNT],
    next: HashMap<String, UriLookupTable>,
}

// Finds the next '/' in s at or after from
fn next_slash(s: &str, from: usize) -> Option<usize> {
    s[from..].find('/').map(|i| i + from)
}

#[derive(Default)]
pub struct Server {
    root_lookup: UriLookupTable,
}

impl Server {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_endpoint(
        &mut self,
        method: Method,
        uri: &str,
        callback: impl Fn(&HttpRequest<'_>) -> HttpResponse + 'static,
    ) -> bool {
        if uri.is_empty() {
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
            return false;
        }

        *handler = Some(Box::new(callback));

        true
    }

    pub fn invoke(&self, request: &HttpRequest<'_>, address: &IpAddress) -> HttpResponse {
        // Make sure that the request is valid

        if request.parsing_error() {
            return HttpResponse::new(Status::BadRequest, "");
        }

        let Some(method) = request.method() else {
            return HttpResponse::new(Status::MethodNotAllowed, "");
        };

        if !SUPPORTED_VERSIONS.contains(&request.version()) {
            return HttpResponse::new(Status::BadRequest, "");
        }

        // Forward the request to the path that will handle it
        // Traverse the path
        let path = request.path();
        let mut state = &self.root_lookup;
        let mut token_start = usize::from(path.starts_with('/'));
        let mut slash = next_slash(path, token_start);

        while let Some(s) = slash.filter(|&s| s != path.len() - 1) {
            let token = &path[token_start..s];
            let Some(next_level) = state.next.get(token) else {
                // Path is not implemented
                return HttpResponse::new(Status::NotFound, "");
            };
            state = next_level;

            token_start = s + 1;
            slash = next_slash(path, token_start);
        }

        // Process final
        if token_start < path.len() {
            let token = &path[token_start..];
            let Some(next_level) = state.next.get(token) else {
                // Path is not implemented
                return HttpResponse::new(Status::NotFound, "");
            };
            state = next_level;
        }

        // Make sure that the method is actually implemented
        let Some(handler) = &state.handlers[method as usize] else {
            return HttpResponse::new(Status::MethodNotAllowed, "");
        };

        // Dispatch
        handler(request)
    }

    pub fn run(&self, port: i32) {
        let mut listener = TcpListener::new(|request: &str, addr: IpAddress| -> String {
            let request = HttpRequest::new(request).expect("Malformed HTTP request");
            self.invoke(&request, &addr).to_string()
        });
        listener.bind(port);
        listener.run();
    }
}
