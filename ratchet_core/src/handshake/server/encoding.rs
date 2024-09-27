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
use crate::handshake::server::{check_partial_request, UpgradeRequest, UpgradeRequestParts};
use crate::handshake::{ParseResult, TryFromWrapper};
use crate::server::parse_request_parts;
use crate::{Error, SubprotocolRegistry};
use bytes::{BufMut, BytesMut};
use http::request::Parts;
use http::{HeaderMap, Request, StatusCode};
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

pub struct RequestParser<E> {
    pub subprotocols: SubprotocolRegistry,
    pub extension: E,
}

impl<E> Decoder for RequestParser<E>
where
    E: ExtensionProvider,
{
    type Item = (UpgradeRequest<E::Extension>, usize);
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

pub fn try_parse_request<'b, E>(
    buffer: &'b [u8],
    mut request: httparse::Request<'b, 'b>,
    extension: E,
    subprotocols: &mut SubprotocolRegistry,
) -> Result<ParseResult<httparse::Request<'b, 'b>, UpgradeRequest<E::Extension>>, Error>
where
    E: ExtensionProvider,
{
    match request.parse(buffer) {
        Ok(Status::Complete(count)) => {
            let request = Request::try_from(TryFromWrapper(request))?;
            let (parts, body) = request.into_parts();
            let Parts {
                method,
                version,
                headers,
                ..
            } = &parts;

            let UpgradeRequestParts {
                key,
                subprotocol,
                extension,
                extension_header,
            } = parse_request_parts(*version, method, headers, extension, subprotocols)?;

            Ok(ParseResult::Complete(
                UpgradeRequest {
                    key,
                    subprotocol,
                    extension,
                    request: Request::from_parts(parts, body),
                    extension_header,
                },
                count,
            ))
        }
        Ok(Status::Partial) => Ok(ParseResult::Partial(request)),
        Err(e) => Err(e.into()),
    }
}
