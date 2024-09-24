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

use crate::handshake::io::BufferedIo;
use crate::handshake::server::HandshakeResult;
use crate::handshake::{negotiate_request, TryMap};
use crate::handshake::{
    validate_header, validate_header_any, validate_header_value, ParseResult, METHOD_GET,
    UPGRADE_STR, WEBSOCKET_STR, WEBSOCKET_VERSION_STR,
};
use crate::{Error, ErrorKind, HttpError, ProtocolRegistry};
use bytes::{BufMut, Bytes, BytesMut};
use http::header::SEC_WEBSOCKET_KEY;
use http::{HeaderMap, Method, StatusCode, Version};
use httparse::Status;
use ratchet_ext::ExtensionProvider;
use tokio::io::AsyncWrite;
use tokio_util::codec::Decoder;

/// The maximum number of headers that will be parsed.
const MAX_HEADERS: usize = 32;
const HTTP_VERSION_STR: &[u8] = b"HTTP/1.1 ";
const STATUS_TERMINATOR_LEN: usize = 2;
const TERMINATOR_NO_HEADERS: &[u8] = b"\r\n\r\n";
const TERMINATOR_WITH_HEADER: &[u8] = b"\r\n";
const HTTP_VERSION_INT: u8 = 1;
const HTTP_VERSION: Version = Version::HTTP_11;

pub struct RequestParser<E> {
    pub subprotocols: ProtocolRegistry,
    pub extension: E,
}

impl<E> Decoder for RequestParser<E>
where
    E: ExtensionProvider,
{
    type Item = (HandshakeResult<E::Extension>, usize);
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let RequestParser {
            subprotocols,
            extension,
        } = self;
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let request = httparse::Request::new(&mut headers);

        match try_parse_request(buf, request, extension, subprotocols)? {
            ParseResult::Complete(result, count) => Ok(Some((result, count))),
            ParseResult::Partial(request) => {
                check_partial_request(&request)?;
                Ok(None)
            }
        }
    }
}

pub async fn write_response<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    status: StatusCode,
    headers: HeaderMap,
    body: Option<String>,
) -> Result<(), Error>
where
    S: AsyncWrite + Unpin,
{
    buf.clear();

    let version_count = HTTP_VERSION_STR.len();
    let status_bytes = status.as_str().as_bytes();
    let reason_len = status
        .canonical_reason()
        .map(|r| r.len() + TERMINATOR_NO_HEADERS.len())
        .unwrap_or(TERMINATOR_WITH_HEADER.len());
    let headers_len = headers.iter().fold(0, |count, (name, value)| {
        name.as_str().len() + value.len() + STATUS_TERMINATOR_LEN + count
    });

    let terminator_len = if headers.is_empty() {
        TERMINATOR_NO_HEADERS.len()
    } else {
        TERMINATOR_WITH_HEADER.len()
    };

    buf.reserve(version_count + status_bytes.len() + reason_len + headers_len + terminator_len);

    buf.put_slice(HTTP_VERSION_STR);
    buf.put_slice(status.as_str().as_bytes());

    match status.canonical_reason() {
        Some(reason) => buf.put_slice(format!(" {} \r\n", reason).as_bytes()),
        None => buf.put_slice(b"\r\n"),
    }

    for (name, value) in &headers {
        buf.put_slice(format!("{}: ", name).as_bytes());
        buf.put_slice(value.as_bytes());
        buf.put_slice(b"\r\n");
    }

    if let Some(body) = body {
        buf.put_slice(body.as_bytes());
    }

    if headers.is_empty() {
        buf.put_slice(TERMINATOR_NO_HEADERS);
    } else {
        buf.put_slice(TERMINATOR_WITH_HEADER);
    }

    let mut buffered = BufferedIo::new(stream, buf);
    buffered.write().await
}

pub fn try_parse_request<'h, 'b, E>(
    buffer: &'b [u8],
    mut request: httparse::Request<'h, 'b>,
    extension: E,
    subprotocols: &mut ProtocolRegistry,
) -> Result<ParseResult<httparse::Request<'h, 'b>, HandshakeResult<E::Extension>>, Error>
where
    E: ExtensionProvider,
{
    match request.parse(buffer) {
        Ok(Status::Complete(count)) => {
            let request = request.try_map()?;
            parse_request(request, extension, subprotocols).map(|r| ParseResult::Complete(r, count))
        }
        Ok(Status::Partial) => Ok(ParseResult::Partial(request)),
        Err(e) => Err(e.into()),
    }
}

pub fn check_partial_request(request: &httparse::Request) -> Result<(), Error> {
    match request.version {
        Some(HTTP_VERSION_INT) | None => {}
        Some(v) => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpVersion(Some(v)),
            ))
        }
    }

    match request.method {
        Some(m) if m.eq_ignore_ascii_case(METHOD_GET) => {}
        None => {}
        m => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpMethod(m.map(ToString::to_string)),
            ));
        }
    }

    Ok(())
}

pub fn parse_request<E>(
    request: http::Request<()>,
    extension: E,
    subprotocols: &mut ProtocolRegistry,
) -> Result<HandshakeResult<E::Extension>, Error>
where
    E: ExtensionProvider,
{
    if request.version() < HTTP_VERSION {
        // this will implicitly be 0 as httparse only parses HTTP/1.x and 1.1 is 0.
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpVersion(Some(0)),
        ));
    }

    if request.method() != Method::GET {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpMethod(Some(request.method().to_string())),
        ));
    }

    let headers = request.headers();
    validate_header_any(headers, http::header::CONNECTION, UPGRADE_STR)?;
    validate_header_value(headers, http::header::UPGRADE, WEBSOCKET_STR)?;
    validate_header_value(
        headers,
        http::header::SEC_WEBSOCKET_VERSION,
        WEBSOCKET_VERSION_STR,
    )?;

    validate_header(headers, http::header::HOST, |_, _| Ok(()))?;

    let key = headers
        .get(SEC_WEBSOCKET_KEY)
        .map(|v| Bytes::from(v.as_bytes().to_vec()))
        .ok_or_else(|| {
            Error::with_cause(ErrorKind::Http, HttpError::MissingHeader(SEC_WEBSOCKET_KEY))
        })?;
    let subprotocol = negotiate_request(subprotocols, headers)?;
    let (extension, extension_header) = extension
        .negotiate_server(headers)
        .map(Option::unzip)
        .map_err(|e| Error::with_cause(ErrorKind::Extension, e))?;

    Ok(HandshakeResult {
        key,
        request,
        extension,
        subprotocol,
        extension_header,
    })
}
