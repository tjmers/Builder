
use builder::server::Server;
use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::logger::LOGGER;

fn main() {
  LOGGER.print_to_stdout();
  let mut s = Server::new(());

  s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, _: &()| -> HttpResponse {
    HttpResponseBuilder::from_text(Status::Ok, "Response").build()
  });

  s.run(2002);
}