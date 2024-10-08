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

//! A library for writing extensions for [Ratchet](../ratchet).
//!
//! # Implementations:
//! [ratchet_deflate](../ratchet_deflate)
//!
//! # Usage
//! Implementing an extension requires two traits to be implemented: [ExtensionProvider] for
//! negotiating the extension during the WebSocket handshake, and [Extension] (along with its
//! bounds) for using the extension during the session.
//!
//! # Splitting an extension
//! If a WebSocket is to be split into its sending and receiving halves then the extension must
//! implement the `SplittableExtension` trait and if it is to be reunited then it must implement the
//! `ReunitableExtension`. This allows more fine-grained control over the BiLock within the
//! receiver.

#![deny(
    missing_docs,
    missing_debug_implementations,
    unused_imports,
    unused_import_braces
)]

pub use http::{HeaderMap, HeaderValue};
pub use httparse::Header;

use bytes::BytesMut;
use std::error::Error;
use std::fmt::Debug;

/// A trait for negotiating an extension during a WebSocket handshake.
///
/// Extension providers allow for a single configuration to be used to negotiate multiple peers.
pub trait ExtensionProvider {
    /// The extension produced by this provider if the negotiation was successful.
    type Extension: Extension;
    /// The error produced by this extension if the handshake failed.
    type Error: Error + Sync + Send + 'static;

    /// Apply this extension's headers to a request.
    fn apply_headers(&self, headers: &mut HeaderMap);

    /// Negotiate the headers that the server responded with.
    ///
    /// If it is possible to negotiate this extension, then this should return an initialised
    /// extension.
    ///
    /// If it is not possible to negotiate an extension then this should return `None`, not `Err`.
    /// An error should only be returned if the server responded with a malformatted header or a
    /// value that was not expected.
    ///
    /// Returning `Err` from this will *fail* the connection with the reason being the error's
    /// `to_string()` value.
    fn negotiate_client(&self, headers: &HeaderMap)
        -> Result<Option<Self::Extension>, Self::Error>;

    /// Negotiate the headers that a client has sent.
    ///
    /// If it is possible to negotiate this extension, then this should return a pair containing an
    /// initialised extension and a `HeaderValue` to return to the client.
    ///
    /// If it is not possible to negotiate an extension then this should return `None`, not `Err`.
    /// An error should only be returned if the server responded with a malformatted header or a
    /// value that was not expected.
    ///
    /// Returning `Err` from this will *fail* the connection with the reason being the error's
    /// `to_string()` value.
    fn negotiate_server(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<(Self::Extension, HeaderValue)>, Self::Error>;
}

impl<'r, E> ExtensionProvider for &'r mut E
where
    E: ExtensionProvider,
{
    type Extension = E::Extension;
    type Error = E::Error;

    fn apply_headers(&self, headers: &mut HeaderMap) {
        E::apply_headers(self, headers)
    }

    fn negotiate_client(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<Self::Extension>, Self::Error> {
        E::negotiate_client(self, headers)
    }

    fn negotiate_server(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<(Self::Extension, HeaderValue)>, Self::Error> {
        E::negotiate_server(self, headers)
    }
}

impl<'r, E> ExtensionProvider for &'r E
where
    E: ExtensionProvider,
{
    type Extension = E::Extension;
    type Error = E::Error;

    fn apply_headers(&self, headers: &mut HeaderMap) {
        E::apply_headers(self, headers)
    }

    fn negotiate_client(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<Self::Extension>, Self::Error> {
        E::negotiate_client(self, headers)
    }

    fn negotiate_server(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<(Self::Extension, HeaderValue)>, Self::Error> {
        E::negotiate_server(self, headers)
    }
}

impl<E> ExtensionProvider for Option<E>
where
    E: ExtensionProvider,
{
    type Extension = E::Extension;
    type Error = E::Error;

    fn apply_headers(&self, headers: &mut HeaderMap) {
        if let Some(provider) = self {
            provider.apply_headers(headers);
        }
    }

    fn negotiate_client(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<Self::Extension>, Self::Error> {
        match self {
            Some(ext) => ext.negotiate_client(headers),
            None => Ok(None),
        }
    }

    fn negotiate_server(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<(Self::Extension, HeaderValue)>, Self::Error> {
        match self {
            Some(ext) => ext.negotiate_server(headers),
            None => Ok(None),
        }
    }
}

/// A data code for a frame.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OpCode {
    /// The message is a continuation.
    Continuation,
    /// The message is text.
    Text,
    /// The message is binary.
    Binary,
}

impl OpCode {
    /// Returns whether this `OpCode` is a continuation.
    pub fn is_continuation(&self) -> bool {
        matches!(self, OpCode::Continuation)
    }

    /// Returns whether this `OpCode` is text.
    pub fn is_text(&self) -> bool {
        matches!(self, OpCode::Text)
    }

    /// Returns whether this `OpCode` is binary.
    pub fn is_binary(&self) -> bool {
        matches!(self, OpCode::Binary)
    }
}

/// A frame's header.
///
/// This is passed to both `ExtensionEncoder::encode` and `ExtensionDecoder::decode` when a frame
/// has been received. Changes to the reserved bits on a decode call will be sent to the peer.
/// Any other changes or changes made when decoding will have no effect.
#[derive(Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Whether this is the final frame.
    ///
    /// Changing this field has no effect.
    pub fin: bool,
    /// Whether `rsv1` was high.
    pub rsv1: bool,
    /// Whether `rsv2` was high.
    pub rsv2: bool,
    /// Whether `rsv3` was high.
    pub rsv3: bool,
    /// The frame's data code.
    ///
    /// Changing this field has no effect.
    pub opcode: OpCode,
}

/// A structure containing the bits that an extension *may* set high during a session.
///
/// If any bits are received by a peer during a session that are different to what this structure
/// returns then the session is failed.
#[derive(Debug)]
pub struct RsvBits {
    /// Whether `rsv1` is allowed to be high.
    pub rsv1: bool,
    /// Whether `rsv2` is allowed to be high.
    pub rsv2: bool,
    /// Whether `rsv3` is allowed to be high.
    pub rsv3: bool,
}

impl From<RsvBits> for u8 {
    fn from(bits: RsvBits) -> Self {
        let RsvBits { rsv1, rsv2, rsv3 } = bits;
        (rsv1 as u8) << 6 | (rsv2 as u8) << 5 | (rsv3 as u8) << 4
    }
}

/// A negotiated WebSocket extension.
pub trait Extension: ExtensionEncoder + ExtensionDecoder + Debug {
    /// Returns the reserved bits that this extension *may* set high during a session.
    fn bits(&self) -> RsvBits;
}

/// A per-message frame encoder.
pub trait ExtensionEncoder {
    /// The error type produced by this extension if encoding fails.
    type Error: Error + Send + Sync + 'static;

    /// Invoked when a frame has been received.
    ///
    /// # Continuation frames
    /// If this frame is not final or a continuation frame then `payload` will contain all of the
    /// data received up to and including this frame.
    ///
    /// # Note
    /// If a condition is not met an implementation may opt to not encode this frame; such as the
    /// payload length not being large enough to require encoding.
    fn encode(
        &mut self,
        payload: &mut BytesMut,
        header: &mut FrameHeader,
    ) -> Result<(), Self::Error>;
}

/// A per-message frame decoder.
pub trait ExtensionDecoder {
    /// The error type produced by this extension if decoding fails.
    type Error: Error + Send + Sync + 'static;

    /// Invoked when a frame has been received.
    ///
    /// # Continuation frames
    /// If this frame is not final or a continuation frame then `payload` will contain all of the
    /// data received up to and including this frame.
    ///
    /// # Note
    /// If a condition is not met an implementation may opt to not decode this frame; such as the
    /// payload length not being large enough to require decoding.
    fn decode(
        &mut self,
        payload: &mut BytesMut,
        header: &mut FrameHeader,
    ) -> Result<(), Self::Error>;
}

/// A trait for permitting an extension to be split into its encoder and decoder halves. Allowing
/// for a WebSocket to be split into its sender and receiver halves.
pub trait SplittableExtension: Extension {
    /// The type of the encoder.
    type SplitEncoder: ExtensionEncoder + Send + Sync + 'static;
    /// The type of the decoder.
    type SplitDecoder: ExtensionDecoder + Send + Sync + 'static;

    /// Split this extension into its encoder and decoder halves.
    fn split(self) -> (Self::SplitEncoder, Self::SplitDecoder);
}

/// A trait for permitting a matched encoder and decoder to be reunited into an extension.
pub trait ReunitableExtension: SplittableExtension {
    /// Reunite this encoder and decoder back into a single extension.
    fn reunite(encoder: Self::SplitEncoder, decoder: Self::SplitDecoder) -> Self;
}

impl<E> Extension for Option<E>
where
    E: Extension,
{
    fn bits(&self) -> RsvBits {
        match self {
            Some(ext) => ext.bits(),
            None => RsvBits {
                rsv1: false,
                rsv2: false,
                rsv3: false,
            },
        }
    }
}

impl<E> ExtensionEncoder for Option<E>
where
    E: ExtensionEncoder,
{
    type Error = E::Error;

    fn encode(
        &mut self,
        payload: &mut BytesMut,
        header: &mut FrameHeader,
    ) -> Result<(), Self::Error> {
        match self {
            Some(e) => e.encode(payload, header),
            None => Ok(()),
        }
    }
}

impl<E> ExtensionDecoder for Option<E>
where
    E: ExtensionDecoder,
{
    type Error = E::Error;

    fn decode(
        &mut self,
        payload: &mut BytesMut,
        header: &mut FrameHeader,
    ) -> Result<(), Self::Error> {
        match self {
            Some(e) => e.decode(payload, header),
            None => Ok(()),
        }
    }
}

impl<E> ReunitableExtension for Option<E>
where
    E: ReunitableExtension,
{
    fn reunite(encoder: Self::SplitEncoder, decoder: Self::SplitDecoder) -> Self {
        Option::zip(encoder, decoder).map(|(encoder, decoder)| E::reunite(encoder, decoder))
    }
}

impl<E> SplittableExtension for Option<E>
where
    E: SplittableExtension,
{
    type SplitEncoder = Option<E::SplitEncoder>;
    type SplitDecoder = Option<E::SplitDecoder>;

    fn split(self) -> (Self::SplitEncoder, Self::SplitDecoder) {
        match self {
            Some(ext) => {
                let (encoder, decoder) = ext.split();
                (Some(encoder), (Some(decoder)))
            }
            None => (None, None),
        }
    }
}
