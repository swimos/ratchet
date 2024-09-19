// Port of hyper_tunstenite for fastwebsockets.
// https://github.com/de-vri-es/hyper-tungstenite-rs
//
// Copyright 2021, Maarten de Vries maarten@de-vri.es
// BSD 2-Clause "Simplified" License
//
// Copyright 2023 Divy Srivastava <dj.srivastava23@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//todo missing docs

#![deny(
    // missing_docs,
    missing_copy_implementations,
    missing_debug_implementations,
    trivial_numeric_casts,
    unstable_features,
    unused_must_use,
    unused_mut,
    unused_imports,
    unused_import_braces
)]

use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use axum_core::body::Body;
use base64;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use eyre::Report;
use http::HeaderMap;
use hyper::Response;
use hyper_util::rt::TokioIo;
use pin_project::pin_project;
use sha1::Digest;
use sha1::Sha1;

type Error = Report;

#[derive(Debug)]
pub struct IncomingUpgrade {
    key: String,
    headers: HeaderMap,
    on_upgrade: hyper::upgrade::OnUpgrade,
    pub permessage_deflate: bool,
}

impl IncomingUpgrade {
    pub fn upgrade(self) -> Result<(Response<Body>, UpgradeFut), Error> {
        let mut builder = Response::builder()
            .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
            .header(hyper::header::CONNECTION, "upgrade")
            .header(hyper::header::UPGRADE, "websocket")
            .header("Sec-WebSocket-Accept", self.key);

        if self.permessage_deflate {
            builder = builder.header("Sec-WebSocket-Extensions", "permessage-deflate");
        }

        let response = builder
            .body(Body::default())
            .expect("bug: failed to build response");

        let stream = UpgradeFut {
            inner: self.on_upgrade,
            headers: self.headers,
        };

        Ok((response, stream))
    }
}

#[async_trait::async_trait]
impl<S> axum_core::extract::FromRequestParts<S> for IncomingUpgrade
where
    S: Sync,
{
    type Rejection = hyper::StatusCode;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get("Sec-WebSocket-Key")
            .ok_or(hyper::StatusCode::BAD_REQUEST)?;
        if parts
            .headers
            .get("Sec-WebSocket-Version")
            .map(|v| v.as_bytes())
            != Some(b"13".as_slice())
        {
            return Err(hyper::StatusCode::BAD_REQUEST);
        }

        let permessage_deflate = parts
            .headers
            .get("Sec-WebSocket-Extensions")
            .map(|val| {
                val.to_str()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("permessage-deflate")
            })
            .unwrap_or(false);

        let on_upgrade = parts
            .extensions
            .remove::<hyper::upgrade::OnUpgrade>()
            .ok_or(hyper::StatusCode::BAD_REQUEST)?;
        Ok(Self {
            on_upgrade,
            key: sec_websocket_protocol(key.as_bytes()),
            headers: parts.headers.clone(),
            permessage_deflate,
        })
    }
}

/// A future that resolves to a websocket stream when the associated HTTP upgrade completes.
#[pin_project]
#[derive(Debug)]
pub struct UpgradeFut {
    #[pin]
    inner: hyper::upgrade::OnUpgrade,
    pub headers: HeaderMap,
}

impl std::future::Future for UpgradeFut {
    type Output = Result<TokioIo<hyper::upgrade::Upgraded>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        let upgraded = match this.inner.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(x) => x,
        };
        Poll::Ready(upgraded.map(|u| TokioIo::new(u)).map_err(|e| e.into()))
    }
}

fn sec_websocket_protocol(key: &[u8]) -> String {
    let mut sha1 = Sha1::new();
    sha1.update(key);
    sha1.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"); // magic string
    let result = sha1.finalize();
    STANDARD.encode(&result[..])
}
