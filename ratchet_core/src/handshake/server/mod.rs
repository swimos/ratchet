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

mod encoding;
#[cfg(test)]
mod tests;

use crate::handshake::{
    validate_header_any, validate_header_value, METHOD_GET, WEBSOCKET_VERSION_STR,
};
use crate::{
    ext::NoExt,
    handshake::io::BufferedIo,
    handshake::server::encoding::{write_response, RequestParser},
    handshake::{StreamingParser, ACCEPT_KEY},
    handshake::{UPGRADE_STR, WEBSOCKET_STR},
    protocol::Role,
    Error, ErrorKind, HttpError, NoExtProvider, Request, SubprotocolRegistry, WebSocket,
    WebSocketConfig, WebSocketStream,
};
use base64::engine::{general_purpose::STANDARD, Engine};
use bytes::{Bytes, BytesMut};
use http::header::{HOST, SEC_WEBSOCKET_KEY};
use http::request::Parts;
use http::status::InvalidStatusCode;
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri, Version};
use log::{error, trace};
use ratchet_ext::{Extension, ExtensionProvider};
use sha1::{Digest, Sha1};
use std::convert::TryFrom;
use std::iter::FromIterator;

const MSG_HANDSHAKE_COMPLETED: &str = "Server handshake completed";
const MSG_HANDSHAKE_FAILED: &str = "Server handshake failed";
const UPGRADED_MSG: &str = "Upgraded connection";
const REJECT_MSG: &str = "Rejected connection";
const HTTP_VERSION_INT: u8 = 1;

/// A structure representing an upgraded WebSocket session and an optional subprotocol that was
/// negotiated during the upgrade.
#[derive(Debug)]
pub struct UpgradedServer<S, E> {
    /// The original request that the peer sent.
    pub request: Request,
    /// The WebSocket connection.
    pub websocket: WebSocket<S, E>,
    /// An optional subprotocol that was negotiated during the upgrade.
    pub subprotocol: Option<String>,
}

impl<S, E> UpgradedServer<S, E> {
    /// Consume self and take the websocket
    pub fn into_websocket(self) -> WebSocket<S, E> {
        self.websocket
    }
}

/// Execute a server handshake on the provided stream.
///
/// Returns either a `WebSocketUpgrader` that may be used to either accept or reject the peer or an
/// error if the peer's request is malformatted or if an IO error occurs. If the peer is accepted,
/// then `config` will be used for building the `WebSocket`.
pub async fn accept<S>(
    stream: S,
    config: WebSocketConfig,
) -> Result<WebSocketUpgrader<S, NoExt>, Error>
where
    S: WebSocketStream,
{
    accept_with(
        stream,
        config,
        NoExtProvider,
        SubprotocolRegistry::default(),
    )
    .await
}

/// Execute a server handshake on the provided stream. An attempt will be made to negotiate the
/// extension and subprotocols provided.
///
/// Returns either a `WebSocketUpgrader` that may be used to either accept or reject the peer or an
/// error if the peer's request is malformatted or if an IO error occurs. If the peer is accepted,
/// then `config`, `extension` and `subprotocols` will be used for building the `WebSocket`.
pub async fn accept_with<S, E>(
    mut stream: S,
    config: WebSocketConfig,
    extension: E,
    subprotocols: SubprotocolRegistry,
) -> Result<WebSocketUpgrader<S, E::Extension>, Error>
where
    S: WebSocketStream,
    E: ExtensionProvider,
{
    let mut buf = BytesMut::new();
    let mut io = BufferedIo::new(&mut stream, &mut buf);
    let parser = StreamingParser::new(
        &mut io,
        RequestParser {
            subprotocols,
            extension,
        },
    );

    match parser.parse().await {
        Ok(request) => {
            let UpgradeRequest {
                key,
                subprotocol,
                extension,
                request,
                extension_header,
            } = request;

            trace!(
                "{}for: {}. Selected subprotocol: {:?} and extension: {:?}",
                request.uri(),
                MSG_HANDSHAKE_COMPLETED,
                subprotocol,
                extension
            );

            Ok(WebSocketUpgrader {
                key,
                buf,
                stream,
                extension,
                request,
                subprotocol,
                extension_header,
                config,
            })
        }
        Err(e) => {
            error!("{}. Error: {:?}", MSG_HANDSHAKE_FAILED, e);

            match e.downcast_ref::<HttpError>() {
                Some(http_err) => {
                    write_response(
                        &mut stream,
                        &mut buf,
                        StatusCode::BAD_REQUEST,
                        HeaderMap::default(),
                        Some(http_err.to_string()),
                    )
                    .await?;
                    Err(e)
                }
                None => Err(e),
            }
        }
    }
}

/// A response to send to a client if the connection will not be upgraded.
#[derive(Debug)]
pub struct WebSocketResponse {
    status: StatusCode,
    headers: HeaderMap,
}

impl WebSocketResponse {
    /// Attempt to construct a new `WebSocketResponse` from `code`.
    ///
    /// # Errors
    /// Errors if the status code is invalid.
    pub fn new(code: u16) -> Result<WebSocketResponse, InvalidStatusCode> {
        StatusCode::from_u16(code).map(|status| WebSocketResponse {
            status,
            headers: HeaderMap::new(),
        })
    }

    /// Attempt to construct a new `WebSocketResponse` from `code` and `headers.
    ///
    /// # Errors
    /// Errors if the status code is invalid.
    pub fn with_headers<I>(code: u16, headers: I) -> Result<WebSocketResponse, InvalidStatusCode>
    where
        I: IntoIterator<Item = (http::header::HeaderName, http::header::HeaderValue)>,
    {
        Ok(WebSocketResponse {
            status: StatusCode::from_u16(code)?,
            headers: HeaderMap::from_iter(headers),
        })
    }
}

/// Represents a client connection that has been accepted and the upgrade request validated. This
/// may be used to validate the request by a user and opt to either continue the upgrade or reject
/// the connection - such as if the target path does not exist.
#[derive(Debug)]
pub struct WebSocketUpgrader<S, E> {
    request: Request,
    key: Bytes,
    subprotocol: Option<String>,
    buf: BytesMut,
    stream: S,
    extension: Option<E>,
    extension_header: Option<HeaderValue>,
    config: WebSocketConfig,
}

impl<S, E> WebSocketUpgrader<S, E>
where
    S: WebSocketStream,
    E: Extension,
{
    /// The subprotocol that the client has requested.
    pub fn subprotocol(&self) -> Option<&String> {
        self.subprotocol.as_ref()
    }

    /// The URI that the client has requested.
    pub fn uri(&self) -> &Uri {
        self.request.uri()
    }

    /// The original request that the client sent.
    pub fn request(&self) -> &Request {
        &self.request
    }

    /// Attempt to upgrade this to a fully negotiated WebSocket connection.
    ///
    /// # Errors
    /// Errors if there is an IO error.
    pub async fn upgrade(self) -> Result<UpgradedServer<S, E>, Error> {
        self.upgrade_with(HeaderMap::default()).await
    }

    /// Insert `headers` into the response and attempt to upgrade this to a fully negotiated
    /// WebSocket connection.
    ///
    /// # Errors
    /// Errors if there is an IO error.
    pub async fn upgrade_with(self, mut headers: HeaderMap) -> Result<UpgradedServer<S, E>, Error> {
        let WebSocketUpgrader {
            request,
            key,
            subprotocol,
            mut buf,
            mut stream,
            extension,
            extension_header,
            config,
        } = self;

        let mut digest = Sha1::new();
        Digest::update(&mut digest, key);
        Digest::update(&mut digest, ACCEPT_KEY);

        let sec_websocket_accept = STANDARD.encode(digest.finalize());
        headers.insert(
            http::header::SEC_WEBSOCKET_ACCEPT,
            HeaderValue::try_from(sec_websocket_accept)?,
        );
        headers.insert(
            http::header::UPGRADE,
            HeaderValue::from_static(WEBSOCKET_STR),
        );
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static(UPGRADE_STR),
        );
        if let Some(subprotocol) = &subprotocol {
            headers.insert(
                http::header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::try_from(subprotocol)?,
            );
        }
        if let Some(extension_header) = extension_header {
            headers.insert(http::header::SEC_WEBSOCKET_EXTENSIONS, extension_header);
        }

        write_response(
            &mut stream,
            &mut buf,
            StatusCode::SWITCHING_PROTOCOLS,
            headers,
            None,
        )
        .await?;

        buf.clear();

        trace!("{} from {}", UPGRADED_MSG, request.uri());

        Ok(UpgradedServer {
            request,
            websocket: WebSocket::from_upgraded(config, stream, extension, buf, Role::Server),
            subprotocol,
        })
    }

    /// Reject this connection with the response provided.
    ///
    /// # Errors
    /// Errors if there is an IO error.
    pub async fn reject(self, response: WebSocketResponse) -> Result<(), Error> {
        let WebSocketResponse { status, headers } = response;
        let WebSocketUpgrader {
            mut stream,
            mut buf,
            request,
            ..
        } = self;

        trace!("{} from {}", REJECT_MSG, request.uri());

        write_response(&mut stream, &mut buf, status, headers, None).await
    }
}

/// Represents a parsed WebSocket connection upgrade HTTP request without the context of the
/// request that it is responding to.
#[derive(Debug)]
#[non_exhaustive]
pub struct UpgradeRequestParts<E> {
    /// The security key provided by the client during the WebSocket handshake.
    ///
    /// This key is used by the server to generate a response key,  confirming that the server
    /// accepts the WebSocket upgrade request.
    pub key: Bytes,

    /// The optional WebSocket subprotocol agreed upon during the handshake.
    ///
    /// The subprotocol is used to define the application-specific communication on top of the
    /// WebSocket connection, such as `wamp` or `graphql-ws`. If no subprotocol is requested or
    /// agreed upon, this will be `None`.
    pub subprotocol: Option<String>,

    /// The optional WebSocket extension negotiated during the handshake.
    ///
    /// Extensions allow WebSocket connections to have additional functionality, such as compression
    /// or multiplexing. This field represents any such negotiated extension, or `None` if no
    /// extensions were negotiated.
    pub extension: Option<E>,

    /// The optional `Sec-WebSocket-Extensions` header value from the HTTP request.
    ///
    /// This header may contain the raw extension details sent by the client during  the handshake.
    /// If no extension was requested, this field will be `None`.
    pub extension_header: Option<HeaderValue>,
}

/// Represents a parsed WebSocket connection upgrade HTTP request.
#[derive(Debug)]
#[non_exhaustive]
pub struct UpgradeRequest<E, B = ()> {
    /// The security key provided by the client during the WebSocket handshake.
    ///
    /// This key is used by the server to generate a response key,  confirming that the server
    /// accepts the WebSocket upgrade request.
    pub key: Bytes,

    /// The optional WebSocket subprotocol agreed upon during the handshake.
    ///
    /// The subprotocol is used to define the application-specific communication on top of the
    /// WebSocket connection, such as `wamp` or `graphql-ws`. If no subprotocol is requested or
    /// agreed upon, this will be `None`.
    pub subprotocol: Option<String>,

    /// The optional WebSocket extension negotiated during the handshake.
    ///
    /// Extensions allow WebSocket connections to have additional functionality, such as compression
    /// or multiplexing. This field represents any such negotiated extension, or `None` if no
    /// extensions were negotiated.
    pub extension: Option<E>,

    /// The original HTTP request that initiated the WebSocket upgrade.
    pub request: http::Request<B>,

    /// The optional `Sec-WebSocket-Extensions` header value from the HTTP request.
    ///
    /// This header may contain the raw extension details sent by the client during  the handshake.
    /// If no extension was requested, this field will be `None`.
    pub extension_header: Option<HeaderValue>,
}

/// Builds an HTTP response to a WebSocket connection upgrade request.
///
/// No validation is performed by this function and it is only guaranteed to be correct if the
/// arguments are derived by previously calling [`parse_request_parts`].
///
/// # Arguments
///
/// - `key`: The WebSocket security key provided by the client in the handshake request.
/// - `subprotocol`: An optional subprotocol that the server and client agree upon, if any.
///   This header is added only if a subprotocol is provided.
/// - `extension_header`: An optional WebSocket extension header. If present, the header
///   is included to specify any negotiated WebSocket extensions.
///
/// # Returns
///
/// This function returns an `http::Response<()>` that represents a valid HTTP 101 Switching
/// Protocols response required to establish a WebSocket connection. The response includes:
///
/// - `Sec-WebSocket-Accept`: The hashed and encoded value of the provided WebSocket `key`
///   according to the WebSocket protocol specification.
/// - `Upgrade`: Set to `websocket`, indicating that the connection is being upgraded to
///   the WebSocket protocol.
/// - `Connection`: Set to `Upgrade`, as required by the HTTP upgrade process.
///
/// Optionally, the response headers may also include:
///
/// - `Sec-WebSocket-Protocol`: The negotiated subprotocol, if provided.
/// - `Sec-WebSocket-Extensions`: The WebSocket extension header, if an extension was negotiated.
///
/// # Errors
///
/// This function returns an `Error` if:
/// - There is an issue converting the headers (like subprotocol or extension) to
///   valid `HeaderValue` types.
/// - There is an issue building the HTTP response.
pub fn build_response(
    key: Bytes,
    subprotocol: Option<String>,
    extension_header: Option<HeaderValue>,
) -> Result<Response<()>, Error> {
    let mut response = http::Response::builder()
        .version(Version::HTTP_11)
        .status(StatusCode::SWITCHING_PROTOCOLS);

    if let Some(headers) = response.headers_mut() {
        *headers = build_response_headers(key, subprotocol, extension_header)?;
    }

    Ok(response.body(())?)
}

/// Processes a WebSocket handshake request and generates the appropriate response.
///
/// This function handles the server-side part of a WebSocket handshake. It parses the incoming HTTP
/// request that seeks to upgrade the connection to WebSocket, negotiates extensions and
/// subprotocols, and constructs an appropriate HTTP response
/// to complete the WebSocket handshake.
///
/// # Arguments
///
/// - `request`: The incoming HTTP request from the client, which contains headers related to the
///  WebSocket upgrade request.
/// - `extension`: An extension that may be negotiated for the connection.
/// - `subprotocols`: A `SubprotocolRegistry`, which will be used to attempt to negotiate a
/// subprotocol.
///
/// # Returns
///
/// This function returns a `Result` containing:
/// - A tuple consisting of:
///   - An `http::Response<()>`, which represents the WebSocket handshake response.
///     The response includes headers such as `Sec-WebSocket-Accept` to confirm the upgrade.
///   - An optional `E::Extension`, which represents the negotiated extension, if any.
///
/// If the handshake fails, an `Error` is returned, which may be caused by invalid
/// requests, issues parsing headers, or problems negotiating the WebSocket subprotocols
/// or extensions.
///
/// # Type Parameters
///
/// - `E`: The type of the extension provider, which must implement the `ExtensionProvider`
///   trait. This defines how WebSocket extensions (like compression) are handled.
/// - `B`: The body type of the HTTP request. While it is discouraged for GET requests to have a body
///  it is not technically incorrect and the use of this function is lowering the guardrails to
///  allow for Ratchet to be more easily integrated into other libraries. It is the implementors
///  responsibility to perform any validation on the body.
///
/// # Errors
///
/// The function returns an `Error` in cases such as:
/// - Failure to parse the WebSocket upgrade request.
/// - Issues building the response, such as invalid subprotocol or extension headers.
/// - Failure to negotiate the WebSocket extensions or subprotocols.
pub fn handshake<E, B>(
    request: http::Request<B>,
    extension: E,
    subprotocols: &SubprotocolRegistry,
) -> Result<(Response<()>, Option<E::Extension>), Error>
where
    E: ExtensionProvider,
{
    let (parts, _body) = request.into_parts();
    let Parts {
        method,
        version,
        headers,
        ..
    } = parts;
    let UpgradeRequestParts {
        key,
        subprotocol,
        extension,
        extension_header,
        ..
    } = parse_request_parts(version, &method, &headers, extension, subprotocols)?;
    Ok((
        build_response(key, subprotocol, extension_header)?,
        extension,
    ))
}

/// Generates a WebSocket upgrade response from the provided headers.
///
/// # Arguments
///
/// - `headers`: A reference to the request's `HeaderMap` containing the HTTP headers. These headers
///  must include the necessary WebSocket headers such as `Sec-WebSocket-Key` and `Upgrade`.
/// - `extension`: An extension that may be negotiated for the connection.
/// - `subprotocols`: A `SubprotocolRegistry`, which will be used to attempt to negotiate a
/// subprotocol.
///
/// # Returns
///
/// This function returns a `Result` containing:
/// - A tuple consisting of:
///   - An `http::Response<()>`, which represents the WebSocket handshake response.
///     The response includes headers such as `Sec-WebSocket-Accept` to confirm the upgrade.
///   - An optional `E::Extension`, which represents the negotiated extension, if any.
///
/// If the handshake fails, an `Error` is returned, which may be caused by invalid
/// requests, issues parsing headers, or problems negotiating the WebSocket subprotocols
/// or extensions.
///
/// # Type Parameters
///
/// - `E`: The type of the extension provider, which must implement the `ExtensionProvider`
///   trait. This defines how WebSocket extensions (like compression) are handled.
///
/// # Errors
///
/// The function returns an `Error` in cases such as:
/// - Failure to parse the WebSocket upgrade request.
/// - Issues building the response, such as invalid subprotocol or extension headers.
/// - Failure to negotiate the WebSocket extensions or subprotocols.
pub fn response_from_headers<E>(
    headers: &HeaderMap,
    extension: E,
    subprotocols: &SubprotocolRegistry,
) -> Result<(Response<()>, Option<E::Extension>), Error>
where
    E: ExtensionProvider,
{
    let UpgradeRequestParts {
        key,
        subprotocol,
        extension,
        extension_header,
    } = parse_request_parts(
        Version::HTTP_11,
        &Method::GET,
        headers,
        extension,
        subprotocols,
    )?;

    let mut response = http::Response::builder()
        .version(Version::HTTP_11)
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .body(())?;
    *response.headers_mut() = build_response_headers(key, subprotocol, extension_header)?;

    Ok((response, extension))
}

/// Constructs the HTTP response headers for a WebSocket handshake response.
///
/// This function builds the necessary headers for completing a WebSocket handshake, including
/// the `Sec-WebSocket-Accept` header, which is derived by hashing the client's WebSocket key
/// and appending the WebSocket GUID. Additionally, it adds optional headers for subprotocols and
/// extensions if they were negotiated during the handshake.
///
/// # Arguments
/// - `key`: The WebSocket key provided by the client as part of the handshake request (`Sec-WebSocket-Key`).
/// - `subprotocol`: An optional `String` that represents the WebSocket subprotocol negotiated between
///   the client and server. If provided, it will be included in the `Sec-WebSocket-Protocol` header.
/// - `extension_header`: An optional `HeaderValue` representing any WebSocket extensions negotiated
///   during the handshake. If provided, it will be added to the `Sec-WebSocket-Extensions` header.
///
/// # Returns
/// - `Result<HeaderMap, Error>`: A result that contains either a `HeaderMap` with the constructed
///  headers or an `Error` if an issue occurs while creating the headers.
pub fn build_response_headers(
    key: Bytes,
    subprotocol: Option<String>,
    extension_header: Option<HeaderValue>,
) -> Result<HeaderMap, Error> {
    let mut digest = Sha1::new();
    Digest::update(&mut digest, key);
    Digest::update(&mut digest, ACCEPT_KEY);

    let sec_websocket_accept = STANDARD.encode(digest.finalize());

    let mut map = HeaderMap::default();

    map.insert(
        http::header::SEC_WEBSOCKET_ACCEPT,
        HeaderValue::try_from(sec_websocket_accept)?,
    );
    map.insert(
        http::header::UPGRADE,
        HeaderValue::from_static(WEBSOCKET_STR),
    );
    map.insert(
        http::header::CONNECTION,
        HeaderValue::from_static(UPGRADE_STR),
    );
    if let Some(subprotocol) = subprotocol {
        map.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::try_from(subprotocol)?,
        );
    }
    if let Some(extension_header) = extension_header {
        map.insert(http::header::SEC_WEBSOCKET_EXTENSIONS, extension_header);
    }

    Ok(map)
}

/// Parses an HTTP request from its parts to extract WebSocket upgrade information.
///
/// This function validates and processes an incoming HTTP request to ensure it meets the
/// requirements for a WebSocket upgrade. It checks the HTTP version, method, and necessary headers
/// to determine if the request can be successfully upgraded to a WebSocket connection. It also
/// negotiates the subprotocols and extensions specified in the request.
///
/// # Arguments
/// - `version`: The HTTP version of the request.
/// - `method`: The HTTP method of the request.
/// - `headers`: A reference to the request's `HeaderMap` containing the HTTP headers. These headers
///  must include the necessary WebSocket headers such as `Sec-WebSocket-Key` and `Upgrade`.
/// - `extension`: An instance of a type that implements the `ExtensionProvider`
/// trait. This object is responsible for negotiating any server-supported
/// extensions requested by the client.
/// - `subprotocols`: A `SubprotocolRegistry`, which manages the supported subprotocols and attempts
/// to negotiate one with the client.
///
/// # Returns
/// This function returns a `Result<UpgradeRequestParts<E::Extension>, Error>`, where:
/// - `Ok(UpgradeRequestParts)`: Contains the parsed information needed for the WebSocket
///   handshake, including the WebSocket key, negotiated subprotocol, and an optional extension.
/// - `Err(Error)`: Contains an error if the request parts are invalid or cannot be parsed.
///   This could include issues such as unsupported HTTP versions, invalid methods,
///   missing required headers, or failed negotiations for subprotocols or extensions.
pub fn parse_request_parts<E>(
    version: Version,
    method: &Method,
    headers: &HeaderMap,
    extension: E,
    subprotocols: &SubprotocolRegistry,
) -> Result<UpgradeRequestParts<E::Extension>, Error>
where
    E: ExtensionProvider,
{
    validate_method_and_version(version, method)?;
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

    Ok(UpgradeRequestParts {
        key,
        extension,
        subprotocol,
        extension_header,
    })
}

fn check_partial_request(request: &httparse::Request) -> Result<(), Error> {
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

/// Validates that `version` and `method` are correct for a WebSocket upgrade.
///
/// # Returns
/// `Ok(())` if they are correct or `Err(e)` if they are not.
pub fn validate_method_and_version(version: Version, method: &Method) -> Result<(), Error> {
    if version < Version::HTTP_11 {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpVersion(format!("{:?}", Version::HTTP_10)),
        ));
    }

    if method != Method::GET {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpMethod(Some(method.to_string())),
        ));
    }

    Ok(())
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
