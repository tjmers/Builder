// actix-web serving the benchmark routes, on its default worker count

use actix_web::{web, App, HttpResponse, HttpServer};
use framework_benches::{port_arg, BACKLOG, JSON_MESSAGE, PLAINTEXT, POST_BODY};

async fn plaintext() -> HttpResponse {
    HttpResponse::Ok().content_type("text/plain").body(PLAINTEXT)
}

// Taking the body as Bytes makes actix read all of it
async fn post_body(_body: web::Bytes) -> HttpResponse {
    HttpResponse::Ok().content_type("text/plain").body(&POST_BODY[..])
}

async fn json() -> HttpResponse {
    HttpResponse::Ok().json(&JSON_MESSAGE)
}

fn main() {
    let port = port_arg();
    actix_web::rt::System::new().block_on(async move {
        HttpServer::new(|| {
            App::new()
                .route("/plaintext", web::get().to(plaintext))
                .route("/post", web::post().to(post_body))
                .route("/json", web::get().to(json))
        })
        .backlog(BACKLOG)
        .bind(("127.0.0.1", port))
        .expect("bind")
        .run()
        .await
        .expect("actix server");
    });
}
