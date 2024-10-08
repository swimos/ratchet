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

mod frame;
mod mask;

pub use frame::*;
pub use mask::apply_mask;

use bytes::Bytes;
use std::convert::TryFrom;
use std::fmt::{Display, Formatter};
use thiserror::Error;

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub struct HeaderFlags: u8 {
        const FIN       = 0b1000_0000;

        const RSV_1     = 0b0100_0000;
        const RSV_2     = 0b0010_0000;
        const RSV_3     = 0b0001_0000;

        // The extension bits that *may* be high. Anything outside this range is illegal.
        const RESERVED  = Self::RSV_1.bits() | Self::RSV_2.bits() | Self::RSV_3.bits();

        // no new flags should be added
    }
}

#[allow(warnings)]
impl HeaderFlags {
    pub fn is_fin(&self) -> bool {
        self.contains(HeaderFlags::FIN)
    }

    pub fn is_rsv1(&self) -> bool {
        self.contains(HeaderFlags::RSV_1)
    }

    pub fn is_rsv2(&self) -> bool {
        self.contains(HeaderFlags::RSV_2)
    }

    pub fn is_rsv3(&self) -> bool {
        self.contains(HeaderFlags::RSV_3)
    }
}

/// A received WebSocket frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// A text message.
    ///
    /// # Note
    /// [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455) is not strict as to when UTF-8
    /// validation takes place. As such, Ratchet opts to not validate the text payload and leaves it
    /// to the user to validate it once the message has been received.
    Text,
    /// A binary message.
    Binary,
    /// A ping message.
    Ping(Bytes),
    /// A pong message.
    Pong(Bytes),
    /// A close message.
    Close(Option<CloseReason>),
}

impl Message {
    /// Whether this is a text message.
    pub fn is_text(&self) -> bool {
        matches!(self, Message::Text)
    }

    /// Whether this is a binary message.
    pub fn is_binary(&self) -> bool {
        matches!(self, Message::Binary)
    }

    /// Whether this is a ping message.
    pub fn is_ping(&self) -> bool {
        matches!(self, Message::Ping(_))
    }

    /// Whether this is a pong message.
    pub fn is_pong(&self) -> bool {
        matches!(self, Message::Pong(_))
    }

    /// Whether this is a close message.
    pub fn is_close(&self) -> bool {
        matches!(self, Message::Close(_))
    }
}

/// The type of a payload to send to a peer.
#[derive(Copy, Clone, Debug)]
pub enum PayloadType {
    /// A text message.
    Text,
    /// A binary message.
    Binary,
    /// A ping message.
    Ping,
    /// A pong message.
    Pong,
}

/// A message type to send.
#[derive(Copy, Clone, Debug)]
pub enum MessageType {
    /// A text message.
    Text,
    /// A binary message.
    Binary,
}

/// A configuration for building a WebSocket.
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub struct WebSocketConfig {
    /// The maximum payload size that is permitted to be received.
    pub max_message_size: usize,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        WebSocketConfig {
            max_message_size: 64 << 20,
        }
    }
}

/// The role of a WebSocket.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Role {
    /// The WebSocket is a client.
    Client,
    /// The WebSocket is a server.
    Server,
}

impl Role {
    /// Returns whether this WebSocket is a client.
    pub fn is_client(&self) -> bool {
        matches!(self, Role::Client)
    }

    /// Returns whether this WebSocket is a server.
    pub fn is_server(&self) -> bool {
        matches!(self, Role::Server)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OpCode {
    DataCode(DataCode),
    ControlCode(ControlCode),
}

impl Display for OpCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl OpCode {
    pub fn is_data(&self) -> bool {
        matches!(self, OpCode::DataCode(_))
    }

    pub fn is_control(&self) -> bool {
        matches!(self, OpCode::ControlCode(_))
    }
}

impl From<OpCode> for u8 {
    fn from(op: OpCode) -> Self {
        match op {
            OpCode::DataCode(code) => code as u8,
            OpCode::ControlCode(code) => code as u8,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DataCode {
    Continuation = 0,
    Text = 1,
    Binary = 2,
}

impl Display for DataCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<DataCode> for ratchet_ext::OpCode {
    fn from(e: DataCode) -> Self {
        match e {
            DataCode::Continuation => ratchet_ext::OpCode::Continuation,
            DataCode::Text => ratchet_ext::OpCode::Text,
            DataCode::Binary => ratchet_ext::OpCode::Binary,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ControlCode {
    Close = 8,
    Ping = 9,
    Pong = 10,
}

impl Display for ControlCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Copy, Clone, Debug, Error, PartialEq, Eq)]
pub enum OpCodeParseErr {
    #[error("Reserved OpCode: `{0}`")]
    Reserved(u8),
    #[error("Invalid OpCode: `{0}`")]
    Invalid(u8),
}

impl TryFrom<u8> for OpCode {
    type Error = OpCodeParseErr;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(OpCode::DataCode(DataCode::Continuation)),
            1 => Ok(OpCode::DataCode(DataCode::Text)),
            2 => Ok(OpCode::DataCode(DataCode::Binary)),
            r @ 3..=7 => Err(OpCodeParseErr::Reserved(r)),
            8 => Ok(OpCode::ControlCode(ControlCode::Close)),
            9 => Ok(OpCode::ControlCode(ControlCode::Ping)),
            10 => Ok(OpCode::ControlCode(ControlCode::Pong)),
            r @ 11..=15 => Err(OpCodeParseErr::Reserved(r)),
            e => Err(OpCodeParseErr::Invalid(e)),
        }
    }
}

/// A reason for closing the WebSocket connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReason {
    /// The code to close the connection with.
    pub code: CloseCode,
    /// An optional message to close the connection with.
    pub description: Option<String>,
}

impl CloseReason {
    #[allow(missing_docs)]
    pub fn new(code: CloseCode, description: Option<String>) -> Self {
        CloseReason { code, description }
    }
}

/// # Additional implementation sources:
/// <https://developer.mozilla.org/en-US/docs/Web/API/CloseEvent>
/// <https://mailarchive.ietf.org/arch/msg/hybi/P_1vbD9uyHl63nbIIbFxKMfSwcM/>
/// <https://tools.ietf.org/id/draft-ietf-hybi-thewebsocketprotocol-09.html>
#[allow(missing_docs)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CloseCode {
    Normal,
    GoingAway,
    Protocol,
    Unsupported,
    Status,
    Abnormal,
    Invalid,
    Policy,
    Overflow,
    Extension,
    Unexpected,
    Restarting,
    TryAgain,
    Tls,
    ReservedExtension(u16),
    Library(u16),
    Application(u16),
}

impl CloseCode {
    pub(crate) fn is_illegal(&self) -> bool {
        matches!(
            self,
            CloseCode::Status
                | CloseCode::Tls
                | CloseCode::Abnormal
                | CloseCode::ReservedExtension(_)
        )
    }
}

#[derive(Copy, Clone, Error, Debug)]
#[error("Unknown close code: `{0}`")]
pub struct CloseCodeParseErr(pub(crate) u16);

impl TryFrom<[u8; 2]> for CloseCode {
    type Error = CloseCodeParseErr;

    fn try_from(value: [u8; 2]) -> Result<Self, Self::Error> {
        let value = u16::from_be_bytes(value);
        match value {
            n @ 0..=999 => Err(CloseCodeParseErr(n)),
            1000 => Ok(CloseCode::Normal),
            1001 => Ok(CloseCode::GoingAway),
            1002 => Ok(CloseCode::Protocol),
            1003 => Ok(CloseCode::Unsupported),
            1005 => Ok(CloseCode::Status),
            1006 => Ok(CloseCode::Abnormal),
            1007 => Ok(CloseCode::Invalid),
            1008 => Ok(CloseCode::Policy),
            1009 => Ok(CloseCode::Overflow),
            1010 => Ok(CloseCode::Extension),
            1011 => Ok(CloseCode::Unexpected),
            1012 => Ok(CloseCode::Restarting),
            1013 => Ok(CloseCode::TryAgain),
            1015 => Ok(CloseCode::Tls),
            n @ 1016..=1999 => Err(CloseCodeParseErr(n)),
            n @ 2000..=2999 => Ok(CloseCode::ReservedExtension(n)),
            n @ 3000..=3999 => Ok(CloseCode::Library(n)),
            n @ 4000..=4999 => Ok(CloseCode::Application(n)),
            n => Err(CloseCodeParseErr(n)),
        }
    }
}

impl From<CloseCode> for u16 {
    fn from(code: CloseCode) -> u16 {
        match code {
            CloseCode::Normal => 1000,
            CloseCode::GoingAway => 1001,
            CloseCode::Protocol => 1002,
            CloseCode::Unsupported => 1003,
            CloseCode::Status => 1005,
            CloseCode::Abnormal => 1006,
            CloseCode::Invalid => 1007,
            CloseCode::Policy => 1008,
            CloseCode::Overflow => 1009,
            CloseCode::Extension => 1010,
            CloseCode::Unexpected => 1011,
            CloseCode::Restarting => 1012,
            CloseCode::TryAgain => 1013,
            CloseCode::Tls => 1015,
            CloseCode::ReservedExtension(n) => n,
            CloseCode::Library(n) => n,
            CloseCode::Application(n) => n,
        }
    }
}
