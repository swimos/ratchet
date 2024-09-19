use axum::{response::IntoResponse, routing::get, Router};
use bytes::BytesMut;
use ratchet_axum::{IncomingUpgrade, UpgradeFut};
use ratchet_core::{Message, NegotiatedExtension, NoExt, PayloadType, Role, WebSocketConfig};

#[tokio::main]
async fn main() {
    let app = Router::new().route("/", get(ws_handler));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn handle_client(fut: UpgradeFut) {
    let io = fut.await.unwrap();
    let mut websocket = ratchet_rs::WebSocket::from_upgraded(
        WebSocketConfig::default(),
        io,
        NegotiatedExtension::from(NoExt),
        BytesMut::new(),
        Role::Server,
    );
    let mut buf = BytesMut::new();

    loop {
        match websocket.read(&mut buf).await.unwrap() {
            Message::Text => {
                websocket.write(&mut buf, PayloadType::Text).await.unwrap();
                buf.clear();
            }
            _ => break,
        }
    }
}

async fn ws_handler(incoming_upgrade: IncomingUpgrade) -> impl IntoResponse {
    let (response, fut) = incoming_upgrade.upgrade().unwrap();
    tokio::task::spawn(async move { handle_client(fut).await });
    response
}
