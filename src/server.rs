use std::collections::HashMap;

use crate::http_request::{HttpRequest, Method};
use crate::http_response::{HttpResponse, Status};
use crate::ip_address::IpAddress;
use crate::tcp_listener::TcpListener;

pub type RequestHandler = Box<dyn fn(&HttpRequest<'_>) -> HttpResponse>;

const VERSIONS_SUPPORTED: [&str; 2] = ["HTTP/1.0", "HTTP/1.1"];
const VERSION_GREATEST: &str = "HTTP/1.1";

#[derive(Default)]
struct UriLookupTable {
    handlers: [Option<RequestHandler>; Method::COUNT],
    next: HashMap<String, UriLookupTable>,
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

        *handler = Some(Box::new(callback));

        true
    }

    pub fn invoke(&self, request: &HttpRequest<'_>, address: &IpAddress) -> HttpResponse {
        // Make sure that the request is valid

        if request.parsing_error() {
            log_error!("400 Bad Request: could not parse request for {}", request.path());
            return HttpResponse::new(Status::BadRequest, "");
        }

        let Some(method) = request.method() else {
            log_error!("405 Method Not Allowed: unrecognized method for {}", request.path());
            return HttpResponse::new(Status::MethodNotAllowed, "");
        };

        if !VERSIONS_SUPPORTED.contains(&request.version()) {
            log_error!("400 Bad Request: unsupported version {:?} for {}", request.version(), request.path());
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
                log_error!("404 Not Found: {:?} {}", method, path);
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
                log_error!("404 Not Found: {:?} {}", method, path);
                return HttpResponse::new(Status::NotFound, "");
            };
            state = next_level;
        }

        // Make sure that the method is actually implemented
        let Some(handler) = &state.handlers[method as usize] else {
            if any_valid_methods(&state.handlers) {
                log_error!("405 Method Not Allowed: {:?} {}", method, path);
                return HttpResponse::new(Status::MethodNotAllowed, "");
            } else {
                // We return not found if we are at an intermediate path with no methods defined
                log_error!("404 Not Found: {:?} {}", method, path);
                return HttpResponse::new(Status::NotFound, "");
            }
        };

        // Dispatch
        let response = handler(request);
        log_info!("{:?} {} -> {}", method, path, response.status() as u16);
        response
    }

    pub fn run(&self, port: i32) {
        let mut listener = TcpListener::new(|request: &str, addr: IpAddress| -> String {
            let request = match HttpRequest::new(request) {
                Ok(req) => req,
                Err(e) => {
                    log_error!("400 Bad Request: {}", e);
                    return HttpResponse::new(Status::BadRequest, "").to_string();
                }
            };
            self.invoke(&request, &addr).to_string()
        });
        listener.bind(port);
        listener.run();
    }
}
