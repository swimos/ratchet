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

use crate::Error;
use bytes::BytesMut;
use http::{HeaderMap, HeaderValue};
use httparse::Header;
use ratchet_ext::{
    Extension, ExtensionDecoder, ExtensionEncoder, ExtensionProvider, FrameHeader,
    ReunitableExtension, RsvBits, SplittableExtension,
};
use std::convert::Infallible;

/// An extension stub that does nothing.
#[derive(Debug, Default, Copy, Clone)]
pub struct NoExt;

impl ExtensionEncoder for NoExt {
    type Error = Infallible;

    fn encode(
        &mut self,
        _payload: &mut BytesMut,
        _header: &mut FrameHeader,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl ExtensionDecoder for NoExt {
    type Error = Infallible;

    fn decode(
        &mut self,
        _payload: &mut BytesMut,
        _header: &mut FrameHeader,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// An extension provider stub that will always succeed with `NoExt`.
#[derive(Copy, Clone, Debug)]
pub struct NoExtProvider;
impl ExtensionProvider for NoExtProvider {
    type Extension = NoExt;
    type Error = Infallible;

    fn apply_headers(&self, _headers: &mut HeaderMap) {}

    fn negotiate_client(
        &self,
        _headers: &[Header],
    ) -> Result<Option<Self::Extension>, Self::Error> {
        Ok(None)
    }

    fn negotiate_server(
        &self,
        _headers: &[Header],
    ) -> Result<Option<(Self::Extension, HeaderValue)>, Self::Error> {
        Ok(None)
    }
}

impl From<Infallible> for Error {
    fn from(e: Infallible) -> Self {
        match e {}
    }
}

impl Extension for NoExt {
    fn bits(&self) -> RsvBits {
        RsvBits {
            rsv1: false,
            rsv2: false,
            rsv3: false,
        }
    }
}

impl SplittableExtension for NoExt {
    type SplitEncoder = NoExtEncoder;
    type SplitDecoder = NoExtDecoder;

    fn split(self) -> (Self::SplitEncoder, Self::SplitDecoder) {
        (NoExtEncoder, NoExtDecoder)
    }
}

impl ReunitableExtension for NoExt {
    fn reunite(_encoder: Self::SplitEncoder, _decoder: Self::SplitDecoder) -> Self {
        NoExt
    }
}

#[derive(Copy, Clone, Debug)]
#[allow(missing_docs)]
pub struct NoExtEncoder;
impl ExtensionEncoder for NoExtEncoder {
    type Error = Infallible;

    fn encode(
        &mut self,
        _payload: &mut BytesMut,
        _header: &mut FrameHeader,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Copy, Clone, Debug)]
#[allow(missing_docs)]
pub struct NoExtDecoder;
impl ExtensionDecoder for NoExtDecoder {
    type Error = Infallible;

    fn decode(
        &mut self,
        _payload: &mut BytesMut,
        _header: &mut FrameHeader,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}
