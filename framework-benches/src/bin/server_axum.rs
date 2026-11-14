// axum (tokio + hyper) serving the benchmark routes, on tokio's default worker count

use axum::body::Bytes;
use axum::routing::{get, post};
use axum::{Json, Router};
use framework_benches::{port_arg, JsonMessage, BACKLOG, JSON_MESSAGE, PLAINTEXT, POST_BODY};
use tokio::net::TcpSocket;

async fn plaintext() -> ([(&'static str, &'static str); 1], &'static str) {
    ([("content-type", "text/plain")], PLAINTEXT)
}

// Taking the body as Bytes makes axum read all of it
async fn post_body(_body: Bytes) -> ([(&'static str, &'static str); 1], Bytes) {
    ([("content-type", "text/plain")], Bytes::from_static(&POST_BODY))
}

async fn json() -> Json<JsonMessage> {
    Json(JSON_MESSAGE)
}

fn main() {
    let port = port_arg();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(async move {
            let socket = TcpSocket::new_v4().expect("create socket");
            socket.set_reuseaddr(true).expect("set SO_REUSEADDR");
            socket.bind(([127, 0, 0, 1], port).into()).expect("bind");
            let listener = socket.listen(BACKLOG).expect("listen");

            let app = Router::new()
                .route("/plaintext", get(plaintext))
                .route("/post", post(post_body))
                .route("/json", get(json));
            axum::serve(listener, app).await.expect("axum server");
        });
}
