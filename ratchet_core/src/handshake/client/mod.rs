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

mod encoding;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bytes::BytesMut;
use http::{header, Request, StatusCode, Version};
use httparse::{Response, Status};
use log::{error, trace};
use sha1::{Digest, Sha1};
use std::convert::TryFrom;

use crate::errors::{Error, ErrorKind, HttpError};
use crate::handshake::client::encoding::{build_request, encode_request};
use crate::handshake::io::BufferedIo;
use crate::handshake::{
    validate_header, validate_header_value, ParseResult, StreamingParser, SubprotocolRegistry,
    TryFromWrapper, ACCEPT_KEY, BAD_STATUS_CODE, UPGRADE_STR, WEBSOCKET_STR,
};
use crate::{
    NoExt, NoExtProvider, Role, TryIntoRequest, WebSocket, WebSocketConfig, WebSocketStream,
};
use http::header::LOCATION;
use log::warn;
use ratchet_ext::ExtensionProvider;
use tokio_util::codec::Decoder;

type Nonce = [u8; 24];

const MSG_HANDSHAKE_COMPLETED: &str = "Client handshake completed";
const MSG_HANDSHAKE_FAILED: &str = "Client handshake failed";

/// A structure representing an upgraded WebSocket session and an optional subprotocol that was
/// negotiated during the upgrade.
#[derive(Debug)]
pub struct UpgradedClient<S, E> {
    /// The WebSocket connection.
    pub websocket: WebSocket<S, E>,
    /// An optional subprotocol that was negotiated during the upgrade.
    pub subprotocol: Option<String>,
}

impl<S, E> UpgradedClient<S, E> {
    /// Consume self and take the websocket
    pub fn into_websocket(self) -> WebSocket<S, E> {
        self.websocket
    }
}

/// Execute a WebSocket client handshake on `stream`, opting for no compression on messages and no
/// subprotocol.
pub async fn subscribe<S, R>(
    config: WebSocketConfig,
    mut stream: S,
    request: R,
) -> Result<UpgradedClient<S, NoExt>, Error>
where
    S: WebSocketStream,
    R: TryIntoRequest,
{
    let mut read_buffer = BytesMut::new();
    let HandshakeResult {
        subprotocol,
        extension,
    } = exec_client_handshake(
        &mut stream,
        request.try_into_request()?,
        NoExtProvider,
        SubprotocolRegistry::default(),
        &mut read_buffer,
    )
    .await?;

    Ok(UpgradedClient {
        websocket: WebSocket::from_upgraded(config, stream, extension, read_buffer, Role::Client),
        subprotocol,
    })
}

/// Execute a WebSocket client handshake on `stream`, attempting to negotiate the extension and a
/// subprotocol.
pub async fn subscribe_with<S, E, R>(
    config: WebSocketConfig,
    mut stream: S,
    request: R,
    extension: E,
    subprotocols: SubprotocolRegistry,
) -> Result<UpgradedClient<S, E::Extension>, Error>
where
    S: WebSocketStream,
    E: ExtensionProvider,
    R: TryIntoRequest,
{
    let mut read_buffer = BytesMut::new();
    let HandshakeResult {
        subprotocol,
        extension,
    } = exec_client_handshake(
        &mut stream,
        request.try_into_request()?,
        extension,
        subprotocols,
        &mut read_buffer,
    )
    .await?;

    Ok(UpgradedClient {
        websocket: WebSocket::from_upgraded(config, stream, extension, read_buffer, Role::Client),
        subprotocol,
    })
}

async fn exec_client_handshake<S, E>(
    stream: &mut S,
    request: Request<()>,
    extension: E,
    subprotocols: SubprotocolRegistry,
    buf: &mut BytesMut,
) -> Result<HandshakeResult<E::Extension>, Error>
where
    S: WebSocketStream,
    E: ExtensionProvider,
{
    let machine = ClientHandshake::new(stream, subprotocols, &extension, buf);
    let uri = request.uri().to_string();
    let handshake_result = machine.exec(request).await;
    match &handshake_result {
        Ok(HandshakeResult {
            subprotocol,
            extension,
            ..
        }) => {
            trace!(
                "{} for: {}. Selected subprotocol: {:?} and extension: {:?}",
                MSG_HANDSHAKE_COMPLETED,
                uri,
                subprotocol,
                extension
            )
        }
        Err(e) => {
            error!("{} for {}. Error: {:?}", MSG_HANDSHAKE_FAILED, uri, e)
        }
    }

    handshake_result
}

struct ClientHandshake<'s, S, E> {
    buffered: BufferedIo<'s, S>,
    nonce: Nonce,
    subprotocols: SubprotocolRegistry,
    extension: &'s E,
}

pub struct StreamingResponseParser<'b, E> {
    nonce: &'b Nonce,
    extension: &'b E,
    subprotocols: &'b mut SubprotocolRegistry,
}

impl<'b, E> Decoder for StreamingResponseParser<'b, E>
where
    E: ExtensionProvider,
{
    type Item = (HandshakeResult<E::Extension>, usize);
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let StreamingResponseParser {
            nonce,
            extension,
            subprotocols,
        } = self;

        let mut headers = [httparse::EMPTY_HEADER; 32];
        let response = Response::new(&mut headers);

        match try_parse_response(buf, response, nonce, extension, subprotocols)? {
            ParseResult::Complete(result, count) => Ok(Some((result, count))),
            ParseResult::Partial(response) => {
                check_partial_response(&response)?;
                Ok(None)
            }
        }
    }
}

impl<'s, S, E> ClientHandshake<'s, S, E>
where
    S: WebSocketStream,
    E: ExtensionProvider,
{
    pub fn new(
        socket: &'s mut S,
        subprotocols: SubprotocolRegistry,
        extension: &'s E,
        buf: &'s mut BytesMut,
    ) -> ClientHandshake<'s, S, E> {
        ClientHandshake {
            buffered: BufferedIo::new(socket, buf),
            nonce: [0; 24],
            subprotocols,
            extension,
        }
    }

    fn encode(&mut self, request: Request<()>) -> Result<(), Error> {
        let ClientHandshake {
            buffered,
            nonce,
            extension,
            subprotocols,
        } = self;

        trace!("Encoding request: {request:?}");
        let validated_request = build_request(request, extension, subprotocols)?;
        encode_request(buffered.buffer, validated_request, nonce);
        Ok(())
    }

    async fn write(&mut self) -> Result<(), Error> {
        trace!("Writing buffered data");
        self.buffered.write().await
    }

    fn clear_buffer(&mut self) {
        self.buffered.clear();
    }

    async fn read(&mut self) -> Result<HandshakeResult<E::Extension>, Error> {
        let ClientHandshake {
            buffered,
            nonce,
            subprotocols,
            extension,
        } = self;

        let parser = StreamingParser::new(
            buffered,
            StreamingResponseParser {
                nonce,
                extension,
                subprotocols,
            },
        );

        parser.parse().await
    }

    // This is split up on purpose so that the individual functions can be called in unit tests.
    pub async fn exec(
        mut self,
        request: Request<()>,
    ) -> Result<HandshakeResult<E::Extension>, Error> {
        self.encode(request)?;
        self.write().await?;
        self.clear_buffer();
        self.read().await
    }
}

#[derive(Debug)]
pub struct HandshakeResult<E> {
    pub subprotocol: Option<String>,
    pub extension: Option<E>,
}

/// Quickly checks a partial response in the order of the expected HTTP response declaration to see
/// if it's valid.
fn check_partial_response(response: &Response) -> Result<(), Error> {
    match response.version {
        // httparse sets this to 0 for HTTP/1.0 or 1 for HTTP/1.1
        // rfc6455 § 4.2.1.1: must be HTTP/1.1 or higher
        Some(1) | None => {}
        Some(_) => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpVersion(format!("{:?}", Version::HTTP_10)),
            ))
        }
    }

    match response.code {
        Some(code) if code == StatusCode::SWITCHING_PROTOCOLS => Ok(()),
        Some(code) if (300..400).contains(&code) => {
            // This keeps the response parsing going until the resource is found and then the
            // upgrade will fail with the redirection location
            Ok(())
        }
        Some(code) => match StatusCode::try_from(code) {
            Ok(code) => Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::Status(code.as_u16()),
            )),
            Err(_) => Err(Error::with_cause(ErrorKind::Http, BAD_STATUS_CODE)),
        },
        None => Ok(()),
    }
}

fn try_parse_response<'b, E>(
    buffer: &'b [u8],
    mut response: Response<'b, 'b>,
    expected_nonce: &Nonce,
    extension: E,
    subprotocols: &mut SubprotocolRegistry,
) -> Result<ParseResult<Response<'b, 'b>, HandshakeResult<E::Extension>>, Error>
where
    E: ExtensionProvider,
{
    match response.parse(buffer) {
        Ok(Status::Complete(count)) => parse_response(
            TryFromWrapper(response).try_into()?,
            expected_nonce,
            extension,
            subprotocols,
        )
        .map(|r| ParseResult::Complete(r, count)),
        Ok(Status::Partial) => Ok(ParseResult::Partial(response)),
        Err(e) => Err(e.into()),
    }
}

fn parse_response<E>(
    response: http::Response<()>,
    expected_nonce: &Nonce,
    extension: E,
    subprotocols: &SubprotocolRegistry,
) -> Result<HandshakeResult<E::Extension>, Error>
where
    E: ExtensionProvider,
{
    if response.version() < Version::HTTP_11 {
        // rfc6455 § 4.2.1.1: must be HTTP/1.1 or higher
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpVersion(format!("{:?}", Version::HTTP_10)),
        ));
    }

    let status_code = response.status();
    match status_code {
        c if c == StatusCode::SWITCHING_PROTOCOLS => {}
        c if c.is_redirection() => {
            return match response.headers().get(LOCATION) {
                Some(value) => {
                    // the value _should_ be valid UTF-8
                    let location = String::from_utf8(value.as_bytes().to_vec())
                        .map_err(|_| Error::new(ErrorKind::Http))?;
                    Err(Error::with_cause(
                        ErrorKind::Http,
                        HttpError::Redirected(location),
                    ))
                }
                None => {
                    warn!("Received a redirection status code with no location header");
                    Err(Error::with_cause(
                        ErrorKind::Http,
                        HttpError::Status(c.as_u16()),
                    ))
                }
            };
        }
        status_code => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::Status(status_code.as_u16()),
            ))
        }
    }

    validate_header_value(response.headers(), header::UPGRADE, WEBSOCKET_STR)?;
    validate_header_value(response.headers(), header::CONNECTION, UPGRADE_STR)?;

    validate_header(
        response.headers(),
        header::SEC_WEBSOCKET_ACCEPT,
        |_name, actual| {
            let mut digest = Sha1::new();
            digest.update(expected_nonce);
            digest.update(ACCEPT_KEY);

            let expected = STANDARD.encode(digest.finalize());
            if expected.as_bytes() != actual {
                Err(Error::with_cause(ErrorKind::Http, HttpError::KeyMismatch))
            } else {
                Ok(())
            }
        },
    )?;

    Ok(HandshakeResult {
        subprotocol: subprotocols.validate_accepted_subprotocol(response.headers())?,
        extension: extension
            .negotiate_client(response.headers())
            .map_err(|e| Error::with_cause(ErrorKind::Extension, e))?,
    })
}
