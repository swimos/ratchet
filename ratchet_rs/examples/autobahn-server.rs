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
use log::trace;
use ratchet_rs::deflate::DeflateExtProvider;
use ratchet_rs::{Error, Message, PayloadType, ProtocolRegistry, WebSocketConfig};
use std::io::Write;
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let f = EnvFilter::new("")
        .add_directive("ratchet_rs=trace".parse().unwrap())
        .add_directive("ratchet_core=trace".parse().unwrap())
        .add_directive("ratchet_deflate=trace".parse().unwrap())
        .add_directive(LevelFilter::TRACE.into());
    tracing_subscriber::fmt().with_env_filter(f).init();

    let addr = "127.0.0.1:9002";
    let listener = TcpListener::bind(&addr).await.unwrap();

    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            println!("Test result {:?}", run(stream).await);
        });
    }
}

async fn run(stream: TcpStream) -> Result<(), Error> {
    // std::io::stdout().flush();

    let mut websocket = ratchet_rs::accept_with(
        stream,
        WebSocketConfig::default(),
        DeflateExtProvider::default(),
        ProtocolRegistry::default(),
    )
    .await
    .unwrap()
    .upgrade()
    .await?
    .websocket;

    let mut buf = BytesMut::new();

    loop {
        match websocket.read(&mut buf).await? {
            Message::Text => {
                let _s = String::from_utf8(buf.to_vec())?;
                websocket.write(&mut buf, PayloadType::Text).await?;
                buf.clear();
            }
            Message::Binary => {
                websocket.write(&mut buf, PayloadType::Binary).await?;
                buf.clear();
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => {
                trace!("Test received close frame: {}", websocket.is_closed());
                return Ok(());
            }
        }
    }
}
