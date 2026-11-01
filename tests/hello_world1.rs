use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, Status};
use builder::ip_address::IpAddress;
use builder::server::Server;

#[test]
fn hello_world1() {
    let mut s = Server::new(());
    s.add_endpoint(Method::Get, "/", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "Hello, World!", request.version())
    });
    s.add_endpoint(Method::Get, "/about", |request: &HttpRequest, _state: &()| -> HttpResponse {
        HttpResponse::new(Status::Ok, "My name is Jacob", request.version())
    });

    // Create a HTTP request and get the reponse
    let request1 = HttpRequest::new(
        "GET / HTTP/1.1\r\nHost: www.example.com\r\nUser-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64)\r\nAccept: text/html\r\n\r\n"
    )
    .unwrap();
    let request2 = HttpRequest::new(
        "GET /about HTTP/1.1\r\nHost: www.example.com\r\nUser-Agent: Mozilla/5.0 (Windows NT 10.1; Win64; x64)\r\nAccept: text/html\r\n\r\n"
    )
    .unwrap();
    let addr = IpAddress::from([127, 0, 0, 1]);

    let a = s.invoke(&request1, &addr);
    let b = s.invoke(&request2, &addr);

    assert!(a.status() as u16 == 200 && a.body() == b"Hello, World!");
    assert!(b.status() as u16 == 200 && b.body() == b"My name is Jacob");
}
