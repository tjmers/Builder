use std::collections::HashMap;

use crate::http_request::{HttpRequest, Method};
use crate::http_response::{HttpResponse, Status};
use crate::ip_address::IpAddress;
use crate::tcp_listener::TcpListener;

pub type RequestHandler<S> = for <'r, 'a> fn(&'r HttpRequest<'a>, &S) -> HttpResponse<'a>;

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
        ) -> HttpResponse<'a> {
        // Make sure that the request is valid

        if request.parsing_error() {
            log_error!("400 Bad Request: could not parse request for {}", request.path());
            return HttpResponse::new(Status::BadRequest, "", "HTTP/1.1");
        }

        let Some(method) = request.method() else {
            log_error!("405 Method Not Allowed: unrecognized method for {}", request.path());
            return HttpResponse::new(Status::MethodNotAllowed, "", "HTTP/1.1");
        };

        if !VERSIONS_SUPPORTED.contains(&request.version()) {
            log_error!("400 Bad Request: unsupported version {:?} for {}", request.version(), request.path());
            return HttpResponse::new(Status::BadRequest, "", "HTTP/1.1");
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
                return HttpResponse::new(Status::NotFound, "", request.version());
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
                return HttpResponse::new(Status::NotFound, "", request.version());
            };
            path_state = next_level;
        }

        // Make sure that the method is actually implemented
        let Some(handler) = &path_state.handlers[method as usize] else {
            if any_valid_methods(&path_state.handlers) {
                log_error!("405 Method Not Allowed: {:?} {}", method, path);
                return HttpResponse::new(Status::MethodNotAllowed, "", request.version());
            } else {
                // We return not found if we are at an intermediate path with no methods defined
                log_error!("404 Not Found: {:?} {}", method, path);
                return HttpResponse::new(Status::NotFound, "", request.version());
            }
        };

        // Dispatch
        let response = handler(request, &self.state);
        log_info!("{:?} {} -> {}", method, path, response.status() as u16);
        response
    }

    pub fn run(&self, port: i32) {
        let mut listener = TcpListener::new(|request: &str, addr: IpAddress| -> String {
            let request = match HttpRequest::new(request) {
                Ok(req) => req,
                Err(e) => {
                    log_error!("400 Bad Request: {}", e);
                    return HttpResponse::new(Status::BadRequest, "", "HTTP/1.1").to_string();
                }
            };
            self.invoke(&request, &addr).to_string()
        });
        listener.bind(port);
        listener.run();
    }
}
