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

#[cfg(test)]
mod tests;

mod client;
mod io;
mod server;
mod subprotocols;

use crate::errors::Error;
use crate::errors::{ErrorKind, HttpError};
use crate::handshake::io::BufferedIo;
use crate::{InvalidHeader, Request};
use http::header::HeaderName;
use http::{HeaderMap, HeaderValue, Method, Version};
use http::{Response, StatusCode, Uri};
use httparse::Header;
use std::str::FromStr;
use tokio::io::AsyncRead;
use tokio_util::codec::Decoder;
use url::Url;

pub use client::{subscribe, subscribe_with, UpgradedClient};
pub use server::{
    accept, accept_with, build_response, handshake, parse_request, UpgradeRequest, UpgradedServer,
    WebSocketResponse, WebSocketUpgrader,
};
pub use subprotocols::*;

const WEBSOCKET_STR: &str = "websocket";
const UPGRADE_STR: &str = "upgrade";
const WEBSOCKET_VERSION_STR: &str = "13";
const BAD_STATUS_CODE: &str = "Invalid status code";
const ACCEPT_KEY: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const METHOD_GET: &str = "get";

pub struct StreamingParser<'i, 'buf, I, P> {
    io: &'i mut BufferedIo<'buf, I>,
    parser: P,
}

impl<'i, 'buf, I, P, O> StreamingParser<'i, 'buf, I, P>
where
    I: AsyncRead + Unpin,
    P: Decoder<Item = (O, usize), Error = Error>,
{
    pub fn new(io: &'i mut BufferedIo<'buf, I>, parser: P) -> StreamingParser<'i, 'buf, I, P> {
        StreamingParser { io, parser }
    }

    pub async fn parse(self) -> Result<O, Error> {
        let StreamingParser { io, mut parser } = self;

        loop {
            io.read().await?;

            match parser.decode(io.buffer) {
                Ok(Some((out, count))) => {
                    io.advance(count);
                    return Ok(out);
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

pub enum ParseResult<R, O> {
    Complete(O, usize),
    Partial(R),
}

/// A trait for creating a request from a type.
pub trait TryIntoRequest {
    /// Attempt to convert this type into a `Request`.
    fn try_into_request(self) -> Result<Request, Error>;
}

impl<'a> TryIntoRequest for &'a str {
    fn try_into_request(self) -> Result<Request, Error> {
        self.parse::<Uri>()?.try_into_request()
    }
}

impl<'a> TryIntoRequest for &'a String {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl TryIntoRequest for String {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl<'a> TryIntoRequest for &'a Uri {
    fn try_into_request(self) -> Result<Request, Error> {
        self.clone().try_into_request()
    }
}

impl TryIntoRequest for Uri {
    fn try_into_request(self) -> Result<Request, Error> {
        Ok(Request::get(self).body(())?)
    }
}

impl<'a> TryIntoRequest for &'a Url {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl TryIntoRequest for Url {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl TryIntoRequest for Request {
    fn try_into_request(self) -> Result<Request, Error> {
        Ok(self)
    }
}

fn validate_header_value(
    headers: &HeaderMap,
    name: HeaderName,
    expected: &str,
) -> Result<(), Error> {
    validate_header(headers, name, |name, actual| {
        if actual.as_bytes().eq_ignore_ascii_case(expected.as_bytes()) {
            Ok(())
        } else {
            Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::InvalidHeader(name),
            ))
        }
    })
}

fn validate_header<F>(headers: &HeaderMap, name: HeaderName, f: F) -> Result<(), Error>
where
    F: Fn(HeaderName, &HeaderValue) -> Result<(), Error>,
{
    match headers.get(&name) {
        Some(value) => f(name, value),
        None => Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::MissingHeader(name),
        )),
    }
}

fn validate_header_any(headers: &HeaderMap, name: HeaderName, expected: &str) -> Result<(), Error> {
    validate_header(headers, name, |name, actual| {
        if actual
            .as_bytes()
            .split(|c| c == &b' ' || c == &b',')
            .any(|v| v.eq_ignore_ascii_case(expected.as_bytes()))
        {
            Ok(())
        } else {
            Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::InvalidHeader(name),
            ))
        }
    })
}

struct TryFromWrapper<T>(pub T);

impl<'h> TryFrom<TryFromWrapper<&'h mut [Header<'h>]>> for HeaderMap {
    type Error = HttpError;

    fn try_from(value: TryFromWrapper<&'h mut [Header<'h>]>) -> Result<Self, Self::Error> {
        let parsed_headers = value.0;

        let mut header_map = HeaderMap::with_capacity(parsed_headers.len());
        for header in parsed_headers {
            let header_string = || {
                let value = String::from_utf8_lossy(header.value);
                format!("{}: {}", header.name, value)
            };

            let name =
                HeaderName::from_str(header.name).map_err(|_| InvalidHeader(header_string()))?;
            let value = HeaderValue::from_bytes(header.value)
                .map_err(|_| InvalidHeader(header_string()))?;
            header_map.insert(name, value);
        }

        Ok(header_map)
    }
}

impl<'b> TryFrom<TryFromWrapper<httparse::Response<'b, 'b>>> for Response<()> {
    type Error = HttpError;

    fn try_from(value: TryFromWrapper<httparse::Response<'b, 'b>>) -> Result<Self, Self::Error> {
        let parsed_response = value.0;

        let mut response = Response::new(());
        let code = match parsed_response.code {
            Some(c) => match StatusCode::from_u16(c) {
                Ok(status) => status,
                Err(_) => {
                    return Err(HttpError::Status(c));
                }
            },
            None => return Err(HttpError::MissingStatus),
        };
        let version = match parsed_response.version {
            Some(v) => match v {
                0 => Version::HTTP_10,
                1 => Version::HTTP_11,
                n => return Err(HttpError::HttpVersion(Some(n))),
            },
            None => return Err(HttpError::HttpVersion(None)),
        };

        *response.headers_mut() = HeaderMap::try_from(TryFromWrapper(parsed_response.headers))?;
        *response.status_mut() = code;
        *response.version_mut() = version;

        Ok(response)
    }
}

impl<'b> TryFrom<TryFromWrapper<httparse::Request<'b, 'b>>> for Request {
    type Error = HttpError;

    fn try_from(value: TryFromWrapper<httparse::Request<'b, 'b>>) -> Result<Self, Self::Error> {
        let parsed_request = value.0;

        let mut request = Request::new(());
        let path = match parsed_request.path {
            Some(path) => path.parse()?,
            None => {
                return Err(HttpError::MalformattedUri(Some(
                    "Missing request path".to_string(),
                )))
            }
        };
        let method = match parsed_request.method {
            Some(m) => {
                Method::from_str(m).map_err(|_| HttpError::HttpMethod(Some(m.to_string())))?
            }
            None => return Err(HttpError::HttpMethod(None)),
        };
        let version = match parsed_request.version {
            Some(v) => match v {
                0 => Version::HTTP_10,
                1 => Version::HTTP_11,
                n => return Err(HttpError::HttpVersion(Some(n))),
            },
            None => return Err(HttpError::HttpVersion(None)),
        };

        *request.headers_mut() = HeaderMap::try_from(TryFromWrapper(parsed_request.headers))?;
        *request.uri_mut() = path;
        *request.version_mut() = version;
        *request.method_mut() = method;

        Ok(request)
    }
}
