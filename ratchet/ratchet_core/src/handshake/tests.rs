// Copyright 2015-2021 SWIM.AI inc.
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

use crate::handshake::{ProtocolError, ProtocolRegistry};
use http::header::SEC_WEBSOCKET_PROTOCOL;

#[test]
fn selects_protocol_ok() {
    let mut headers = [httparse::Header {
        name: SEC_WEBSOCKET_PROTOCOL.as_str(),
        value: b"warp, warps",
    }];
    let request = httparse::Request::new(&mut headers);

    let registry = ProtocolRegistry::new(vec!["warps", "warp"]);
    assert_eq!(
        registry.negotiate_request(&request),
        Ok(Some("warp".to_string()))
    );
}

#[test]
fn multiple_headers() {
    let mut headers = [
        httparse::Header {
            name: SEC_WEBSOCKET_PROTOCOL.as_str(),
            value: b"warp",
        },
        httparse::Header {
            name: SEC_WEBSOCKET_PROTOCOL.as_str(),
            value: b"warps",
        },
    ];
    let request = httparse::Request::new(&mut headers);

    let registry = ProtocolRegistry::new(vec!["warps", "warp"]);
    assert_eq!(
        registry.negotiate_request(&request),
        Ok(Some("warp".to_string()))
    );
}

#[test]
fn mixed_headers() {
    let mut headers = [
        httparse::Header {
            name: SEC_WEBSOCKET_PROTOCOL.as_str(),
            value: b"warp1.0",
        },
        httparse::Header {
            name: SEC_WEBSOCKET_PROTOCOL.as_str(),
            value: b"warps2.0,warp3.0",
        },
        httparse::Header {
            name: SEC_WEBSOCKET_PROTOCOL.as_str(),
            value: b"warps4.0",
        },
    ];
    let request = httparse::Request::new(&mut headers);

    let registry = ProtocolRegistry::new(vec!["warps", "warp", "warps2.0"]);
    assert_eq!(
        registry.negotiate_request(&request),
        Ok(Some("warps2.0".to_string()))
    );
}

#[test]
fn malformatted() {
    let mut headers = [httparse::Header {
        name: SEC_WEBSOCKET_PROTOCOL.as_str(),
        value: &[255, 255, 255, 255],
    }];
    let request = httparse::Request::new(&mut headers);

    let registry = ProtocolRegistry::new(vec!["warps", "warp", "warps2.0"]);
    assert_eq!(
        registry.negotiate_request(&request),
        Err(ProtocolError::Encoding)
    );
}

#[test]
fn no_match() {
    let mut headers = [httparse::Header {
        name: SEC_WEBSOCKET_PROTOCOL.as_str(),
        value: b"a,b,c",
    }];
    let request = httparse::Request::new(&mut headers);

    let registry = ProtocolRegistry::new(vec!["d"]);
    assert_eq!(registry.negotiate_request(&request), Ok(None));
}