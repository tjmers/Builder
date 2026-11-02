
use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::server::Server;
use builder::text::{TextFile, TextType};
use builder::logger::LOGGER;

// Host an endpoint on /index that returns H1 with Hello, World

const PAGE1: &str = "<h1>Index</h1><p><a href=\"/about\">Go to about</a></p>";
const PAGE2: &str = "<h1>About</h1><p><a href=\"/index\">Go to index</a></p>";

struct AppState {
    response1: HttpResponse<'static>,
    response2: HttpResponse<'static>,
}

fn main() {
    LOGGER.print_to_stdout();
    // Create the only response beforehand and store it in the app state so that all responses are cached
    let response1 = HttpResponseBuilder::from_text_file(Status::Ok, TextFile::new(PAGE1, TextType::Html)).build();
    let response2 = HttpResponseBuilder::from_text_file(Status::Ok, TextFile::new(PAGE2, TextType::Html)).build();
    let mut s = Server::new(AppState { response1, response2 });

    s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, state: &AppState| -> HttpResponse {
        state.response1.borrowed()
    });
    s.add_endpoint(Method::Get, "/about", |_request: &HttpRequest, state: &AppState| -> HttpResponse {
        state.response2.borrowed()
    });

    s.run(6767);
}
