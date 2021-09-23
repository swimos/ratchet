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

use crate::Error;
use http::{HeaderMap, HeaderValue};
use httparse::Header;
use ratchet_ext::{
    Extension, ExtensionDecoder, ExtensionEncoder, ExtensionProvider, FrameHeader,
    ReunitableExtension, SplittableExtension,
};
use std::convert::Infallible;

#[derive(Debug, Default, Clone)]
pub struct NoExt;

impl ExtensionEncoder for NoExt {
    type Error = Infallible;

    fn encode<A>(&mut self, _payload: A, _header: &mut FrameHeader) -> Result<(), Self::Error>
    where
        A: AsMut<[u8]>,
    {
        Ok(())
    }
}

impl ExtensionDecoder for NoExt {
    type Error = Infallible;

    fn decode<A>(&mut self, _payload: A, _header: &mut FrameHeader) -> Result<(), Self::Error>
    where
        A: AsMut<[u8]>,
    {
        Ok(())
    }
}

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

#[derive(Debug)]
pub struct NoExtEncoder;
impl ExtensionEncoder for NoExtEncoder {
    type Error = Infallible;

    fn encode<A>(&mut self, _payload: A, _header: &mut FrameHeader) -> Result<(), Self::Error>
    where
        A: AsMut<[u8]>,
    {
        Ok(())
    }
}

#[derive(Debug)]
pub struct NoExtDecoder;
impl ExtensionDecoder for NoExtDecoder {
    type Error = Infallible;

    fn decode<A>(&mut self, _payload: A, _header: &mut FrameHeader) -> Result<(), Self::Error>
    where
        A: AsMut<[u8]>,
    {
        Ok(())
    }
}

#[derive(Debug)]
pub struct NegotiatedExtension<E>(Option<E>);
impl<E> NegotiatedExtension<E>
where
    E: Extension,
{
    pub fn take(self) -> Option<E> {
        self.0
    }
}

impl<E> From<Option<E>> for NegotiatedExtension<E>
where
    E: Extension,
{
    fn from(ext: Option<E>) -> Self {
        NegotiatedExtension(ext)
    }
}

impl<E> From<E> for NegotiatedExtension<E>
where
    E: Extension,
{
    fn from(ext: E) -> Self {
        NegotiatedExtension::from(Some(ext))
    }
}

impl<E> ExtensionEncoder for NegotiatedExtension<E>
where
    E: ExtensionEncoder,
{
    type Error = E::Error;

    fn encode<A>(&mut self, payload: A, header: &mut FrameHeader) -> Result<(), Self::Error>
    where
        A: AsMut<[u8]>,
    {
        match &mut self.0 {
            Some(ext) => ext.encode(payload, header),
            None => Ok(()),
        }
    }
}

impl<E> ExtensionDecoder for NegotiatedExtension<E>
where
    E: ExtensionDecoder,
{
    type Error = E::Error;

    fn decode<A>(&mut self, payload: A, header: &mut FrameHeader) -> Result<(), Self::Error>
    where
        A: AsMut<[u8]>,
    {
        match &mut self.0 {
            Some(ext) => ext.decode(payload, header),
            None => Ok(()),
        }
    }
}

impl<E> SplittableExtension for NegotiatedExtension<E>
where
    E: SplittableExtension,
{
    type SplitEncoder = NegotiatedExtension<E::SplitEncoder>;
    type SplitDecoder = NegotiatedExtension<E::SplitDecoder>;

    fn split(self) -> (Self::SplitEncoder, Self::SplitDecoder) {
        match self.0 {
            Some(ext) => {
                let (enc, dec) = ext.split();
                (
                    NegotiatedExtension(Some(enc)),
                    NegotiatedExtension(Some(dec)),
                )
            }
            None => (NegotiatedExtension(None), NegotiatedExtension(None)),
        }
    }
}

impl<E> ReunitableExtension for NegotiatedExtension<E>
where
    E: ReunitableExtension,
{
    fn reunite(encoder: Self::SplitEncoder, decoder: Self::SplitDecoder) -> Self {
        match (encoder.0, decoder.0) {
            (Some(enc), Some(dec)) => NegotiatedExtension(Some(E::reunite(enc, dec))),
            (None, None) => NegotiatedExtension(None),
            _ => panic!("Illegal state"),
        }
    }
}