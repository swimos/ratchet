use bytes::{Bytes, BytesMut};
use tokio::io::{duplex, DuplexStream};
use crate::{Message, NegotiatedExtension, NoExt, PayloadType, Role, WebSocket, WebSocketConfig};

fn fixture()->(WebSocket<DuplexStream,NoExt>,WebSocket<DuplexStream,NoExt>) {
    let (server, client) = duplex(128);
    let config = WebSocketConfig::default();

    let  server = WebSocket::from_upgraded(
        config,
        server,
        NegotiatedExtension::from(NoExt),
        BytesMut::new(),
        Role::Server,
    );
    let client = WebSocket::from_upgraded(
        config,
        client,
        NegotiatedExtension::from(NoExt),
        BytesMut::new(),
        Role::Client,
    );

    (client,server)
}

#[tokio::test]
async fn reads_ping() {
    let (mut client, mut server) = fixture();
    let payload = "ping!";
    client.write_ping(payload).await.expect("Send failed.");

    let mut read_buf = BytesMut::new();
    let message = server.read(&mut read_buf).await.expect("Read failure");

    assert_eq!(message, Message::Ping(Bytes::from("ping!")));
    assert!(read_buf.is_empty());
}
