// tiny_http serving the benchmark routes
// It hands requests out from one queue, so one thread per CPU takes them from it

use std::sync::Arc;
use std::thread;

use framework_benches::{port_arg, std_listener, JSON_MESSAGE, PLAINTEXT, POST_BODY, POST_BODY_BYTES};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

fn respond(request: Request, status: u16, content_type: &Header, body: &[u8]) {
    let response = Response::new(StatusCode(status), vec![content_type.clone()], body, Some(body.len()), None);
    let _ = request.respond(response);
}

fn main() {
    let server = Arc::new(Server::from_listener(std_listener(port_arg()), None).expect("start server"));
    let text = Header::from_bytes("Content-Type", "text/plain").expect("header");
    let json = Header::from_bytes("Content-Type", "application/json").expect("header");

    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (server, text, json) = (Arc::clone(&server), text.clone(), json.clone());
            thread::spawn(move || {
                let mut discard = Vec::with_capacity(POST_BODY_BYTES);
                for mut request in server.incoming_requests() {
                    discard.clear();
                    let _ = request.as_reader().read_to_end(&mut discard);

                    match (request.method(), request.url()) {
                        (Method::Get, "/plaintext") => respond(request, 200, &text, PLAINTEXT.as_bytes()),
                        (Method::Post, "/post") => respond(request, 200, &text, &POST_BODY),
                        (Method::Get, "/json") => {
                            let body = serde_json::to_vec(&JSON_MESSAGE).expect("serialize");
                            respond(request, 200, &json, &body);
                        }
                        _ => respond(request, 404, &text, b""),
                    }
                }
            })
        })
        .collect();

    for worker in workers {
        let _ = worker.join();
    }
}
