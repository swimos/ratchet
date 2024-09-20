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
use http::HeaderMap;
use hyper::Response;
use hyper_util::rt::TokioIo;
use pin_project::pin_project;
use sha1::Digest;
use sha1::Sha1;

const HEADER_CONNECTION: &str = "upgrade";
const HEADER_UPGRADE: &str = "websocket";
const WEBSOCKET_VERSION: &[u8] = b"13";

type Error = hyper::Error;

#[derive(Debug)]
pub struct WebSocketUpgrade {
    key: String,
    headers: HeaderMap,
    on_upgrade: hyper::upgrade::OnUpgrade,
    pub permessage_deflate: bool,
}

impl WebSocketUpgrade {
    pub fn upgrade(self) -> Result<(Response<Body>, UpgradeFut), Error> {
        let mut builder = Response::builder()
            .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
            .header(hyper::header::CONNECTION, HEADER_CONNECTION)
            .header(hyper::header::UPGRADE, HEADER_UPGRADE)
            .header(hyper::header::SEC_WEBSOCKET_ACCEPT, self.key);

        if self.permessage_deflate {
            builder = builder.header(
                hyper::header::SEC_WEBSOCKET_EXTENSIONS,
                "permessage-deflate",
            );
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

    // pub fn upgrade_2<E, F, Fut>(self, f: F) -> Response<Body>
    // where
    //     F: FnOnce(UpgradedServer<TokioIo<hyper::upgrade::Upgraded>, E>) -> Fut,
    //     Fut: Future<Output = ()>,
    //     E: Extension,
    // {
    //
    //
    // }
}

#[async_trait::async_trait]
impl<S> axum_core::extract::FromRequestParts<S> for WebSocketUpgrade
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
            .get(http::header::SEC_WEBSOCKET_KEY)
            .ok_or(hyper::StatusCode::BAD_REQUEST)?;

        if parts
            .headers
            .get(http::header::SEC_WEBSOCKET_VERSION)
            .map(|v| v.as_bytes())
            != Some(WEBSOCKET_VERSION)
        {
            return Err(hyper::StatusCode::BAD_REQUEST);
        }

        let permessage_deflate = parts
            .headers
            .get(http::header::SEC_WEBSOCKET_EXTENSIONS)
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
