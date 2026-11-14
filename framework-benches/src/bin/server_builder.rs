// Builder serving the benchmark routes
// The cached routes return Builder's whole response borrowed from the state, its intended fast path

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::server::Server;
use builder::text::{TextFile, TextType};
use framework_benches::{port_arg, JSON_MESSAGE, PLAINTEXT, POST_BODY_BYTES};

struct State {
    plaintext: HttpResponse<'static>,
    post: HttpResponse<'static>,
}

fn plaintext<'s>(_request: &HttpRequest<'_>, state: &'s State) -> HttpResponse<'s> {
    state.plaintext.borrowed()
}

fn post<'s>(_request: &HttpRequest<'_>, state: &'s State) -> HttpResponse<'s> {
    state.post.borrowed()
}

fn json<'s>(_request: &HttpRequest<'_>, _state: &'s State) -> HttpResponse<'s> {
    let body = serde_json::to_vec(&JSON_MESSAGE).expect("serialize");
    HttpResponseBuilder::from_text_file(Status::Ok, TextFile::new(body, TextType::Json)).build()
}

fn main() {
    let port = port_arg();
    let mut server = Server::new(State {
        plaintext: HttpResponseBuilder::from_text(Status::Ok, PLAINTEXT).build(),
        post: HttpResponseBuilder::from_text(Status::Ok, "a".repeat(POST_BODY_BYTES)).build(),
    });
    server.add_endpoint(Method::Get, "/plaintext", plaintext);
    server.add_endpoint(Method::Post, "/post", post);
    server.add_endpoint(Method::Get, "/json", json);
    server.run(i32::from(port));
}
