// use std::thread;

// use builder::http_request::{HttpRequest, Method};
// use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
// use builder::server::Server;

// fn server_thread(port: i32) {
//     let mut s = Server::new(());
//     s.add_endpoint(Method::Get, "/", |request: &HttpRequest, _state: &()| -> HttpResponse {
//         HttpResponseBuilder::from_text(Status::Ok, "Hello, World!", request.version()).build()
//     });

//     s.
// }

// #[test]
// fn hello_world2() {

// }
