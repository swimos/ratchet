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
use bytes::{BufMut, BytesMut};
use http::header::{AsHeaderName, HeaderName, IntoHeaderName};
use http::request::Parts;
use http::{header, HeaderMap, HeaderValue, Method, Request, Version};

use ratchet_ext::ExtensionProvider;

use crate::errors::{Error, ErrorKind, HttpError};
use crate::handshake::client::Nonce;
use crate::handshake::{SubprotocolRegistry, UPGRADE_STR, WEBSOCKET_STR, WEBSOCKET_VERSION_STR};

use base64::engine::general_purpose::STANDARD;

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
Connection: Upgrade\r\n\
Upgrade: websocket\r\n\
sec-websocket-version: 13\r\n\
sec-websocket-key: {nonce}",
        version = version,
        path = path_and_query,
        host = host,
        nonce = nonce_str
    );

    // 28 = request terminator + nonce buffer len
    let mut len = 28 + request.len();

    let origin = write_header(&headers, header::ORIGIN);
    let protocol = write_header(&headers, header::SEC_WEBSOCKET_PROTOCOL);
    let ext = write_header(&headers, header::SEC_WEBSOCKET_EXTENSIONS);
    let auth = write_header(&headers, header::AUTHORIZATION);

    if let Some((name, value)) = &origin {
        len += name.len() + value.len() + 2;
    }
    if let Some((name, value)) = &protocol {
        len += name.len() + value.len() + 2;
    }
    if let Some((name, value)) = &ext {
        len += name.len() + value.len() + 2;
    }
    if let Some((name, value)) = &auth {
        len += name.len() + value.len() + 2;
    }

    dst.reserve(len);
    dst.put_slice(request.as_bytes());

    if let Some((name, value)) = origin {
        dst.put_slice(b"\r\n");
        dst.put_slice(name.as_bytes());
        dst.put_slice(value);
    }
    if let Some((name, value)) = protocol {
        dst.put_slice(b"\r\n");
        dst.put_slice(name.as_bytes());
        dst.put_slice(value);
    }
    if let Some((name, value)) = ext {
        dst.put_slice(b"\r\n");
        dst.put_slice(name.as_bytes());
        dst.put_slice(value);
    }
    if let Some((name, value)) = auth {
        dst.put_slice(b"\r\n");
        dst.put_slice(name.as_bytes());
        dst.put_slice(value);
    }

    dst.put_slice(b"\r\n\r\n");
}

fn write_header(headers: &HeaderMap<HeaderValue>, name: HeaderName) -> Option<(String, &[u8])> {
    headers
        .get(&name)
        .map(|value| (format!("{}: ", name), value.as_bytes()))
}

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
    subprotocols: &SubprotocolRegistry,
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
            HttpError::HttpVersion(None),
        ));
    }

    let authority = uri
        .authority()
        .ok_or_else(|| Error::with_cause(ErrorKind::Http, "Missing authority"))?
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

    if headers.get(header::SEC_WEBSOCKET_EXTENSIONS).is_some() {
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::InvalidHeader(header::SEC_WEBSOCKET_EXTENSIONS),
        ));
    }

    extension.apply_headers(&mut headers);

    if headers.get(header::SEC_WEBSOCKET_PROTOCOL).is_some() {
        // WebSocket protocols can only be applied using a ProtocolRegistry
        return Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::InvalidHeader(header::SEC_WEBSOCKET_PROTOCOL),
        ));
    }

    subprotocols.apply_to(&mut headers);

    let option = headers
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|head| head.to_str());
    match option {
        Some(Ok(version)) if version == WEBSOCKET_VERSION_STR => {}
        None => {
            headers.insert(
                header::SEC_WEBSOCKET_VERSION,
                HeaderValue::from_static(WEBSOCKET_VERSION_STR),
            );
        }
        _ => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::InvalidHeader(header::SEC_WEBSOCKET_KEY),
            ));
        }
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

fn validate_or_insert<A>(
    headers: &mut HeaderMap,
    header_name: A,
    expected: HeaderValue,
) -> Result<(), Error>
where
    A: AsHeaderName + IntoHeaderName + Clone,
{
    if let Some(header_value) = headers.get(header_name.clone()) {
        match header_value.to_str() {
            Ok(v) if v.as_bytes().eq_ignore_ascii_case(expected.as_bytes()) => Ok(()),
            _ => Err(Error::new(ErrorKind::Http)),
        }
    } else {
        headers.insert(header_name, expected);
        Ok(())
    }
}
