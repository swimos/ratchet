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

use crate::{Error, ErrorKind, HttpError, ProtocolError};
use fnv::FnvHashSet;
use http::header::SEC_WEBSOCKET_PROTOCOL;
use http::{HeaderMap, HeaderValue};
use std::sync::Arc;

/// A subprotocol registry that is used for negotiating a possible subprotocol to use for a
/// connection.
#[derive(Default, Debug, Clone)]
pub struct SubprotocolRegistry {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    registrants: FnvHashSet<String>,
    header: Option<HeaderValue>,
}

impl SubprotocolRegistry {
    /// Construct a new protocol registry that will allow the provided subprotocols. The priority
    /// of the subprotocols is specified by the order that the iterator yields items.
    pub fn new<I>(i: I) -> Result<SubprotocolRegistry, Error>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let registrants = i.into_iter().map(Into::into).collect::<FnvHashSet<_>>();
        let header_str = registrants
            .clone()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        let header = HeaderValue::from_str(&header_str).map_err(|_| {
            Error::with_cause(ErrorKind::Http, HttpError::MalformattedHeader(header_str))
        })?;

        Ok(SubprotocolRegistry {
            inner: Arc::new(Inner {
                registrants,
                header: Some(header),
            }),
        })
    }

    /// Attempts to negotiate a subprotocol offered by a client.
    ///
    /// # Returns
    /// The subprotocol that was negotiated if one was offered. Or an error if the client send a
    /// malformed header.
    pub fn negotiate_client(
        &self,
        header_map: &HeaderMap,
    ) -> Result<Option<String>, ProtocolError> {
        let SubprotocolRegistry { inner } = self;

        for header in header_map.get_all(SEC_WEBSOCKET_PROTOCOL) {
            let header_str = header.to_str().map_err(|_| ProtocolError::Encoding)?;

            for protocol in header_str.split(',') {
                if let Some(supported_protocol) = inner.registrants.get(protocol.trim()) {
                    return Ok(Some(supported_protocol.clone()));
                }
            }
        }

        Ok(None)
    }

    /// Validate a server's response for SEC_WEBSOCKET_PROTOCOL. A server may send at most one
    /// sec-websocket-protocol header, and it must contain a subprotocol that was offered by the
    /// client.
    ///
    /// # Returns
    /// The subprotocol that was accepted by the server if one was offered. Or an error if the
    /// server responded with a malformed header.
    pub fn validate_accepted_subprotocol(
        &self,
        header_map: &HeaderMap,
    ) -> Result<Option<String>, ProtocolError> {
        let SubprotocolRegistry { inner } = self;

        let protocols: Vec<_> = header_map.get_all(SEC_WEBSOCKET_PROTOCOL).iter().collect();

        if protocols.len() > 1 {
            return Err(ProtocolError::InvalidSubprotocolHeader(
                "Server returned too many subprotocols".to_string(),
            ));
        }

        if protocols.is_empty() {
            return Ok(None);
        }

        let server_protocol = protocols[0].to_str().map_err(|_| ProtocolError::Encoding)?;

        if inner.registrants.contains(server_protocol) {
            Ok(Some(server_protocol.to_string()))
        } else {
            Err(ProtocolError::InvalidSubprotocolHeader(
                server_protocol.to_string(),
            ))
        }
    }

    /// Applies a sec-websocket-protocol header to `target` if one has been registered.
    pub fn apply_to(&self, target: &mut HeaderMap) {
        let SubprotocolRegistry { inner } = self;

        if let Some(header) = &inner.header {
            target.insert(SEC_WEBSOCKET_PROTOCOL, header.clone());
        }
    }
}
