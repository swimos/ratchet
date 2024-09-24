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

use crate::handshake::{negotiate_request, ProtocolRegistry};
use crate::ProtocolError;
use http::header::SEC_WEBSOCKET_PROTOCOL;
use http::{HeaderMap, HeaderValue};

#[test]
fn selects_protocol_ok() {
    let headers = HeaderMap::from_iter([(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("warp, warps"),
    )]);
    let registry = ProtocolRegistry::new(vec!["warps", "warp"]).unwrap();

    assert_eq!(
        negotiate_request(&registry, &headers),
        Ok(Some("warp".to_string()))
    );
}

#[test]
fn multiple_headers() {
    let headers = HeaderMap::from_iter([
        (SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("warp")),
        (SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("warps")),
    ]);
    let registry = ProtocolRegistry::new(vec!["warps", "warp"]).unwrap();

    assert_eq!(
        negotiate_request(&registry, &headers),
        Ok(Some("warp".to_string()))
    );
}

#[test]
fn mixed_headers() {
    let headers = HeaderMap::from_iter([
        (SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("warp1.0")),
        (
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("warps2.0,warp3.0"),
        ),
        (SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("warps4.0")),
    ]);
    let registry = ProtocolRegistry::new(vec!["warps", "warp", "warps2.0"]).unwrap();

    assert_eq!(
        negotiate_request(&registry, &headers),
        Ok(Some("warps2.0".to_string()))
    );
}

#[test]
fn malformatted() {
    let headers = HeaderMap::from_iter([(SEC_WEBSOCKET_PROTOCOL, unsafe {
        HeaderValue::from_maybe_shared_unchecked([255, 255, 255, 255])
    })]);
    let registry = ProtocolRegistry::new(vec!["warps", "warp", "warps2.0"]).unwrap();

    assert_eq!(
        negotiate_request(&registry, &headers),
        Err(ProtocolError::Encoding)
    );
}

#[test]
fn no_match() {
    let headers =
        HeaderMap::from_iter([(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("a,b,c"))]);
    let registry = ProtocolRegistry::new(vec!["d"]).unwrap();

    assert_eq!(negotiate_request(&registry, &headers), Ok(None));
}
