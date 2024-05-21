// Copyright 2015-2021 Swim Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use bytes::BytesMut;
use ratchet_rs::deflate::DeflateExtProvider;
use ratchet_rs::{Error, Message, PayloadType, ProtocolRegistry, WebSocketConfig};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() {
    let addr = "127.0.0.1:9003";
    let listener = TcpListener::bind(&addr).await.unwrap();

    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(async move { run(stream).await });
    }
}

async fn run(stream: TcpStream) -> Result<(), Error> {
    let (mut tx, mut rx) = ratchet_rs::accept_with(
        BufReader::new(BufWriter::new(stream)),
        WebSocketConfig::default(),
        DeflateExtProvider::default(),
        ProtocolRegistry::default(),
    )
    .await
    .unwrap()
    .upgrade()
    .await?
    .websocket
    .split()
    .unwrap();

    let mut buf = BytesMut::new();

    loop {
        match rx.read(&mut buf).await? {
            Message::Text => {
                let _s = String::from_utf8(buf.to_vec())?;
                tx.write(&mut buf, PayloadType::Text).await?;
                tx.flush().await?;
                buf.clear();
            }
            Message::Binary => {
                tx.write(&mut buf, PayloadType::Binary).await?;
                tx.flush().await?;
                buf.clear();
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => {
                return Ok(());
            }
        }
    }
}
