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

use base64::Engine;
use bytes::BytesMut;
use http::header::{SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL};
use http::request::Parts;
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, Version};

use ratchet_ext::ExtensionProvider;

use crate::errors::{Error, ErrorKind, HttpError};
use crate::handshake::client::Nonce;
use crate::handshake::{
    apply_to, ProtocolRegistry, UPGRADE_STR, WEBSOCKET_STR, WEBSOCKET_VERSION_STR,
};

use base64::engine::general_purpose::STANDARD;
use log::error;

pub fn encode_request(dst: &mut BytesMut, request: ValidatedRequest, nonce_buffer: &mut Nonce) {
    let ValidatedRequest {
        version,
        headers,
        path_and_query,
        host,
    } = request;

    let nonce = rand::random::<[u8; 16]>();

    // This will only fail due to the buffer being too small but one with sufficient capacity has
    // been allocated.
    STANDARD
        .encode_slice(nonce, nonce_buffer)
        .expect("Encoding should has succeeded");

    let nonce_str = std::str::from_utf8(nonce_buffer).expect("Invalid UTF8");

    let request = format!(
        "\
GET {path} {version:?}\r\n\
Host: {host}\r\n\
sec-websocket-key: {nonce}",
        version = version,
        path = path_and_query,
        host = host,
        nonce = nonce_str
    );

    extend(dst, request.as_bytes());

    for (name, value) in &headers {
        extend(dst, b"\r\n");
        extend(dst, name.as_str().as_bytes());
        extend(dst, b": ");
        extend(dst, value.as_bytes());
    }

    extend(dst, b"\r\n\r\n");
}

#[inline]
fn extend(dst: &mut BytesMut, data: &[u8]) {
    dst.extend_from_slice(data);
}

#[derive(Debug)]
pub struct ValidatedRequest {
    version: Version,
    headers: HeaderMap,
    path_and_query: String,
    host: String,
}

// rfc6455 § 4.2.1
pub fn build_request<E>(
    request: Request<()>,
    extension: &E,
    subprotocols: &ProtocolRegistry,
) -> Result<ValidatedRequest, Error>
where
    E: ExtensionProvider,
{
    let (parts, _body) = request.into_parts();
    let Parts {
        method,
        uri,
        version,
        mut headers,
        ..
    } = parts;

    if method != Method::GET {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpMethod(Some(method.to_string())),
        ));
    }

    if version != Version::HTTP_11 {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::HttpVersion(format!("{version:?}")),
        ));
    }

    if headers.get(SEC_WEBSOCKET_EXTENSIONS).is_some() {
        error!(
            "{} should only be set by extensions",
            SEC_WEBSOCKET_EXTENSIONS
        );
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::InvalidHeader(SEC_WEBSOCKET_EXTENSIONS),
        ));
    }

    // Run this first to ensure that the extension doesn't invalidate the headers.
    extension.apply_headers(&mut headers);

    let authority = uri
        .authority()
        .ok_or_else(|| Error::with_cause(ErrorKind::Http, HttpError::MissingAuthority))?
        .as_str()
        .to_string();
    validate_or_insert(
        &mut headers,
        header::HOST,
        HeaderValue::from_str(authority.as_ref())?,
    )?;

    validate_or_insert(
        &mut headers,
        header::CONNECTION,
        HeaderValue::from_static(UPGRADE_STR),
    )?;
    validate_or_insert(
        &mut headers,
        header::UPGRADE,
        HeaderValue::from_static(WEBSOCKET_STR),
    )?;
    validate_or_insert(
        &mut headers,
        header::SEC_WEBSOCKET_VERSION,
        HeaderValue::from_static(WEBSOCKET_VERSION_STR),
    )?;

    if headers.get(SEC_WEBSOCKET_PROTOCOL).is_some() {
        error!(
            "{} should only be set by extensions",
            SEC_WEBSOCKET_PROTOCOL
        );
        // WebSocket protocols can only be applied using a ProtocolRegistry
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::InvalidHeader(SEC_WEBSOCKET_PROTOCOL),
        ));
    }

    apply_to(subprotocols, &mut headers);

    if headers.get(SEC_WEBSOCKET_KEY).is_some() {
        error!("{} should not be set", SEC_WEBSOCKET_KEY);
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::InvalidHeader(SEC_WEBSOCKET_KEY),
        ));
    }

    let host = uri
        .authority()
        .ok_or_else(|| {
            Error::with_cause(
                ErrorKind::Http,
                HttpError::MalformattedUri(Some("Missing authority".to_string())),
            )
        })?
        .to_string();

    let path_and_query = uri
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_else(|| "/".to_string());

    Ok(ValidatedRequest {
        version,
        headers,
        path_and_query,
        host,
    })
}

fn validate_or_insert(
    headers: &mut HeaderMap,
    header_name: HeaderName,
    expected: HeaderValue,
) -> Result<(), HttpError> {
    if let Some(header_value) = headers.get(header_name.clone()) {
        match header_value.to_str() {
            Ok(v) if v.as_bytes().eq_ignore_ascii_case(expected.as_bytes()) => Ok(()),
            _ => {
                error!("Invalid header set: {} -> {:?}", header_name, header_value);
                Err(HttpError::InvalidHeader(header_name))
            }
        }
    } else {
        headers.insert(header_name, expected);
        Ok(())
    }
}
