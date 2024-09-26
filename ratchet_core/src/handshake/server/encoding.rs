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
use crate::handshake::server::UpgradeRequest;
use crate::handshake::{
    validate_header_any, validate_header_value, ParseResult, TryFromWrapper, METHOD_GET,
    UPGRADE_STR, WEBSOCKET_STR, WEBSOCKET_VERSION_STR,
};
use crate::{Error, ErrorKind, HttpError, SubprotocolRegistry};
use bytes::{BufMut, Bytes, BytesMut};
use http::header::{HOST, SEC_WEBSOCKET_KEY};
use http::{HeaderMap, Method, Request, StatusCode, Version};
use httparse::Status;
use log::error;
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
            parse_request(request, extension, subprotocols).map(|r| ParseResult::Complete(r, count))
        }
        Ok(Status::Partial) => Ok(ParseResult::Partial(request)),
        Err(e) => Err(e.into()),
    }
}

pub fn check_partial_request(request: &httparse::Request) -> Result<(), Error> {
    match request.version {
        Some(HTTP_VERSION_INT) | None => {}
        Some(_) => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpVersion(format!("{:?}", Version::HTTP_10)),
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

/// Parses an HTTP request to extract WebSocket upgrade information.
///
/// This function validates and processes an incoming HTTP request to ensure it meets the
/// requirements for a WebSocket upgrade. It checks the HTTP version, method, and necessary headers
/// to determine if the request can be successfully upgraded to a WebSocket connection. It also
/// negotiates the subprotocols and extensions specified in the request.
///
/// # Arguments
/// - `request`: An `http::Request<B>` representing the incoming HTTP request from the client, which
/// is expected to contain WebSocket-specific headers. While it is discouraged for GET requests to
/// have a body it is not technically incorrect and the use of this function is lowering the
/// guardrails to allow for Ratchet to be more easily integrated into other libraries. It is the
/// implementors responsibility to perform any validation on the body.
/// - `extension`: An instance of a type that implements the `ExtensionProvider`
/// trait. This object is responsible for negotiating any server-supported
/// extensions requested by the client.
/// - `subprotocols`: A `SubprotocolRegistry`, which manages the supported subprotocols and attempts
/// to negotiate one with the client.
///
/// # Returns
/// This function returns a `Result<UpgradeRequest<E::Extension, B>, Error>`, where:
/// - `Ok(UpgradeRequest)`: Contains the parsed information needed for the WebSocket
///   handshake, including the WebSocket key, negotiated subprotocol, optional
///   extensions, and the original HTTP request.
/// - `Err(Error)`: Contains an error if the request is invalid or cannot be parsed.
///   This could include issues such as unsupported HTTP versions, invalid methods,
///   missing required headers, or failed negotiations for subprotocols or extensions.
pub fn parse_request<E, B>(
    request: http::Request<B>,
    extension: E,
    subprotocols: &SubprotocolRegistry,
) -> Result<UpgradeRequest<E::Extension, B>, Error>
where
    E: ExtensionProvider,
{
    if request.version() < HTTP_VERSION {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpVersion(format!("{:?}", Version::HTTP_10)),
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

    if let Err(e) = validate_host_header(headers) {
        error!("Server responded with invalid 'host' headers");
        return Err(e);
    }

    let key = headers
        .get(SEC_WEBSOCKET_KEY)
        .map(|v| Bytes::from(v.as_bytes().to_vec()))
        .ok_or_else(|| {
            Error::with_cause(ErrorKind::Http, HttpError::MissingHeader(SEC_WEBSOCKET_KEY))
        })?;
    let subprotocol = subprotocols.negotiate_client(headers)?;
    let (extension, extension_header) = extension
        .negotiate_server(headers)
        .map(Option::unzip)
        .map_err(|e| Error::with_cause(ErrorKind::Extension, e))?;

    Ok(UpgradeRequest {
        key,
        extension,
        subprotocol,
        extension_header,
        request,
    })
}

/// Validates that 'headers' contains one 'host' header and that it is not a seperated list.
fn validate_host_header(headers: &HeaderMap) -> Result<(), Error> {
    let len = headers
        .iter()
        .filter_map(|(name, value)| {
            if name.as_str().eq_ignore_ascii_case(HOST.as_str()) {
                Some(value.as_bytes().split(|c| c == &b' ' || c == &b','))
            } else {
                None
            }
        })
        .count();
    if len == 1 {
        Ok(())
    } else {
        Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::MissingHeader(HOST),
        ))
    }
}
