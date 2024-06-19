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
use ratchet_deflate::{Deflate, DeflateExtProvider};
use ratchet_rs::UpgradedClient;
use ratchet_rs::{Error, Message, PayloadType, ProtocolRegistry, WebSocketConfig};
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;

const AGENT: &str = "Ratchet";

async fn subscribe(
    url: &str,
) -> Result<UpgradedClient<BufReader<BufWriter<TcpStream>>, Deflate>, Error> {
    let stream = TcpStream::connect("127.0.0.1:9003").await.unwrap();
    stream.set_nodelay(true).unwrap();

    ratchet_rs::subscribe_with(
        WebSocketConfig::default(),
        BufReader::new(BufWriter::new(stream)),
        url,
        &DeflateExtProvider::default(),
        ProtocolRegistry::default(),
    )
    .await
}

async fn get_case_count() -> Result<u32, Error> {
    let mut websocket = subscribe("ws://localhost:9003/getCaseCount")
        .await
        .unwrap()
        .websocket;
    let mut buf = BytesMut::new();

    match websocket.read(&mut buf).await? {
        Message::Text => {
            let count = String::from_utf8(buf.to_vec()).unwrap();
            Ok(count.parse::<u32>().unwrap())
        }
        _ => panic!(),
    }
}

async fn update_reports() -> Result<(), Error> {
    let _websocket = subscribe(&format!(
        "ws://localhost:9003/updateReports?agent={}",
        AGENT
    ))
    .await
    .unwrap();
    Ok(())
}

async fn run_test(case: u32) -> Result<(), Error> {
    let (mut tx, mut rx) = subscribe(&format!(
        "ws://localhost:9003/runCase?case={}&agent={}",
        case, AGENT
    ))
    .await
    .unwrap()
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
            Message::Close(_) => break Ok(()),
        }
    }
}

#[tokio::main]
async fn main() {
    let total = get_case_count().await.unwrap();

    for case in 1..=total {
        let _r = run_test(case).await;
    }

    update_reports().await.unwrap();
}
