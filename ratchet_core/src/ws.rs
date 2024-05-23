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

use crate::errors::{CloseCause, Error, ErrorKind, ProtocolError};
use crate::ext::NegotiatedExtension;
use crate::framed::{FramedIo, Item};
use crate::protocol::{
    CloseReason, ControlCode, DataCode, HeaderFlags, Message, MessageType, OpCode, PayloadType,
    Role,
};
use crate::{WebSocketConfig, WebSocketStream};
use bytes::BytesMut;
use log::{error, trace};
use ratchet_ext::{Extension, ExtensionEncoder, FrameHeader as ExtFrameHeader};

#[cfg(feature = "split")]
use crate::split::{split, Receiver, Sender};
#[cfg(feature = "split")]
use ratchet_ext::SplittableExtension;

pub const CONTROL_MAX_SIZE: usize = 125;

#[cfg(feature = "split")]
type SplitSocket<S, E> = (
    Sender<S, <E as SplittableExtension>::SplitEncoder>,
    Receiver<S, <E as SplittableExtension>::SplitDecoder>,
);

/// An upgraded WebSocket stream.
///
/// This is created after a connection has been upgraded to speak the WebSocket protocol by using
/// the `accept`, `accept_with`, `subscribe` and `subscribe_with` functions or by using
/// `WebSocket::from_upgraded` with an already upgraded stream.
///
/// # Example
/// ```no_run
/// # use ratchet_core::{subscribe, UpgradedClient, Error, Message, PayloadType, WebSocketConfig};
/// # use tokio::net::TcpStream;
/// # use bytes::BytesMut;
///
/// # #[tokio::main]
/// # async fn main()-> Result<(), Error> {
/// let stream = TcpStream::connect("127.0.0.1:9001").await?;
/// let upgraded = subscribe(WebSocketConfig::default(), stream, "ws://127.0.0.1/hello").await?;
/// let UpgradedClient { mut  websocket, .. } = upgraded;
///
/// let mut buf = BytesMut::new();
///
/// loop {
///     match websocket.read(&mut buf).await? {
///         Message::Text => {
///             websocket.write(&mut buf, PayloadType::Text).await?;
///             buf.clear();
///         }
///         Message::Binary => {
///             websocket.write(&mut buf, PayloadType::Binary).await?;
///             buf.clear();
///         }
///         Message::Ping(_) | Message::Pong(_) => {}
///         Message::Close(_) => break Ok(()),
///     }
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct WebSocket<S, E> {
    framed: FramedIo<S>,
    control_buffer: BytesMut,
    extension: NegotiatedExtension<E>,
    close_state: CloseState,
}

/// Denotes the current state of a WebSocket session.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CloseState {
    /// The session is active.
    NotClosed,
    /// Either a user or peer requested closure of the session has been requested.
    Closing,
    /// The WebSocket session is closed.
    Closed,
}

impl<S, E> WebSocket<S, E>
where
    S: WebSocketStream,
    E: Extension,
{
    #[cfg(feature = "split")]
    pub(crate) fn from_parts(
        framed: FramedIo<S>,
        control_buffer: BytesMut,
        extension: NegotiatedExtension<E>,
        close_state: CloseState,
    ) -> WebSocket<S, E> {
        WebSocket {
            framed,
            control_buffer,
            extension,
            close_state,
        }
    }

    /// Initialise a new `WebSocket` from a stream that has already executed a handshake.
    ///
    /// # Arguments
    /// `config` - The configuration to initialise the WebSocket with.
    /// `stream` - The stream that the handshake was executed on.
    /// `extension` - A negotiated extension that will be used for the session.
    /// `read_buffer` - The read buffer which will be used for the session. This **may** contain any
    /// unread data received after performing the handshake that was not required.
    /// `role` - The role that this WebSocket will take.
    pub fn from_upgraded(
        config: WebSocketConfig,
        stream: S,
        extension: NegotiatedExtension<E>,
        read_buffer: BytesMut,
        role: Role,
    ) -> WebSocket<S, E> {
        let WebSocketConfig { max_message_size } = config;
        WebSocket {
            framed: FramedIo::new(
                stream,
                read_buffer,
                role,
                max_message_size,
                extension.bits().into(),
            ),
            extension,
            control_buffer: BytesMut::with_capacity(CONTROL_MAX_SIZE),
            close_state: CloseState::NotClosed,
        }
    }

    /// Returns the role of this WebSocket.
    pub fn role(&self) -> Role {
        if self.framed.is_server() {
            Role::Server
        } else {
            Role::Client
        }
    }

    /// Attempt to read some data from the WebSocket. Returning either the type of the message
    /// received or the error that was produced.
    ///
    /// # Errors
    /// In the event that an error is produced, an attempt is made to cleanly close the connection
    /// by sending a close frame to the peer. If this attempt fails, then the connection is
    /// abruptly closed and the cause of the error is returned.
    ///
    /// In the event that an error is produced the contents of `read_buffer` must be considered to
    /// be dirty; unless the error indicates a clean closure.
    ///
    /// # Control frames
    /// Ratchet transparently handles ping messages received from the peer in read operations by
    /// returning a pong frame and this function will return `Message::Pong` if one has been
    /// received. As per [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455) these may be
    /// interleaved between data frames. In the event of one being received while reading a
    /// continuation, this function will then yield `Message::Ping` and the `read_buffer` will
    /// contain the data received up to that point. The callee must ensure that the contents of
    /// `read_buffer` are **not** then modified before calling `read` again.
    pub async fn read(&mut self, read_buffer: &mut BytesMut) -> Result<Message, Error> {
        if self.is_closed() {
            return Err(Error::with_cause(ErrorKind::Close, CloseCause::Error));
        }

        let WebSocket {
            framed,
            close_state,
            control_buffer,
            extension,
            ..
        } = self;

        match framed.read_next(read_buffer, extension).await {
            Ok(item) => match item {
                Item::Binary => Ok(Message::Binary),
                Item::Text => Ok(Message::Text),
                Item::Ping(payload) => {
                    trace!("Received a ping frame. Responding with pong");
                    let ret = payload.clone().freeze();
                    framed
                        .write(
                            OpCode::ControlCode(ControlCode::Pong),
                            HeaderFlags::FIN,
                            payload,
                            |_, _| Ok(()),
                        )
                        .await?;
                    Ok(Message::Ping(ret))
                }
                Item::Pong(payload) => {
                    if control_buffer.is_empty() {
                        trace!("Received an unsolicited pong frame");
                    } else {
                        control_buffer.clear();
                        trace!("Received pong frame");
                    }
                    Ok(Message::Pong(payload.freeze()))
                }
                Item::Close(reason) => close(close_state, framed, reason, None).await,
            },
            Err(e) => {
                error!("WebSocket read failure: {:?}", e);
                close(close_state, framed, None, Some(e)).await
            }
        }
    }

    /// Constructs a new text WebSocket message with a payload of `data`.
    pub async fn write_text<I>(&mut self, data: I) -> Result<(), Error>
    where
        I: AsRef<str>,
    {
        self.write(data.as_ref(), PayloadType::Text).await
    }

    /// Constructs a new binary WebSocket message with a payload of `data`.
    pub async fn write_binary<I>(&mut self, data: I) -> Result<(), Error>
    where
        I: AsRef<[u8]>,
    {
        self.write(data.as_ref(), PayloadType::Binary).await
    }

    /// Constructs a new ping WebSocket message with a payload of `data`.
    pub async fn write_ping<I>(&mut self, data: I) -> Result<(), Error>
    where
        I: AsRef<[u8]>,
    {
        self.write(data.as_ref(), PayloadType::Ping).await
    }

    /// Constructs a new pong WebSocket message with a payload of `data`.
    pub async fn write_pong<I>(&mut self, data: I) -> Result<(), Error>
    where
        I: AsRef<[u8]>,
    {
        self.write(data.as_ref(), PayloadType::Pong).await
    }

    /// Constructs a new WebSocket message of `message_type` and with a payload of `buf.
    pub async fn write<A>(&mut self, buf: A, message_type: PayloadType) -> Result<(), Error>
    where
        A: AsRef<[u8]>,
    {
        if !self.is_active() {
            return Err(Error::with_cause(ErrorKind::Close, CloseCause::Error));
        }

        let buf = buf.as_ref();

        let op_code = match message_type {
            PayloadType::Text => OpCode::DataCode(DataCode::Text),
            PayloadType::Binary => OpCode::DataCode(DataCode::Binary),
            PayloadType::Ping => {
                if buf.len() > CONTROL_MAX_SIZE {
                    return Err(Error::with_cause(
                        ErrorKind::Protocol,
                        ProtocolError::FrameOverflow,
                    ));
                } else {
                    self.control_buffer.clear();
                    self.control_buffer.extend_from_slice(buf);
                    OpCode::ControlCode(ControlCode::Ping)
                }
            }
            PayloadType::Pong => {
                if buf.len() > CONTROL_MAX_SIZE {
                    return Err(Error::with_cause(
                        ErrorKind::Protocol,
                        ProtocolError::FrameOverflow,
                    ));
                } else {
                    OpCode::ControlCode(ControlCode::Pong)
                }
            }
        };

        let encoder = &mut self.extension;
        self.framed
            .write(op_code, HeaderFlags::FIN, buf, |payload, header| {
                extension_encode(encoder, payload, header)
            })
            .await
    }

    /// Close this WebSocket with the reason provided.
    ///
    /// If the WebSocket is already closed then `Ok(())` is returned.
    pub async fn close(&mut self, reason: CloseReason) -> Result<(), Error> {
        if !self.is_active() {
            return Ok(());
        }

        self.close_state = CloseState::Closing;
        self.framed.write_close(reason).await
    }

    /// Constructs a new WebSocket message of `message_type` and with a payload of `buf_ref` and
    /// chunked by `fragment_size`. If the length of the buffer is less than the chunk size then
    /// only a single message is sent.
    pub async fn write_fragmented<A>(
        &mut self,
        buf: A,
        message_type: MessageType,
        fragment_size: usize,
    ) -> Result<(), Error>
    where
        A: AsRef<[u8]>,
    {
        if !self.is_active() {
            return Err(Error::with_cause(ErrorKind::Close, CloseCause::Error));
        }

        let encoder = &mut self.extension;
        self.framed
            .write_fragmented(buf, message_type, fragment_size, |payload, header| {
                extension_encode(encoder, payload, header)
            })
            .await
    }

    /// Returns whether this WebSocket is closed.
    pub fn is_closed(&self) -> bool {
        self.close_state == CloseState::Closed
    }

    /// Returns whether this WebSocket is closing or closed.
    pub fn is_active(&self) -> bool {
        matches!(self.close_state, CloseState::NotClosed)
    }

    /// Attempt to split the `WebSocket` into its sender and receiver halves.
    ///
    /// # Note
    /// This function does **not** split the IO. It, instead, places the IO into a `BiLock` and
    /// requires exclusive access to it in order to perform any read or write operations.
    ///
    /// In addition to this, the internal framed writer is placed into a `BiLock` so
    /// the receiver half can transparently handle control frames that may be received.
    ///
    /// See: [Tokio#3200](https://github.com/tokio-rs/tokio/issues/3200) and [Tokio#40](https://github.com/tokio-rs/tls/issues/40)
    ///
    /// # Errors
    /// This function will only error if the `WebSocket` is already closed.
    #[cfg(feature = "split")]
    pub fn split(self) -> Result<SplitSocket<S, E>, Error>
    where
        E: SplittableExtension,
    {
        if self.is_closed() {
            Err(Error::with_cause(ErrorKind::Close, CloseCause::Error))
        } else {
            let WebSocket {
                framed,
                control_buffer,
                extension,
                ..
            } = self;
            Ok(split(framed, control_buffer, extension))
        }
    }
}

async fn close<S>(
    close_state: &mut CloseState,
    framed: &mut FramedIo<S>,
    reason: Option<CloseReason>,
    ret: Option<Error>,
) -> Result<Message, Error>
where
    S: WebSocketStream,
{
    let server = framed.is_server();
    match *close_state {
        CloseState::NotClosed => {
            let mut code = match &reason {
                Some(reason) => u16::from(reason.code).to_be_bytes(),
                None => [0; 2],
            };

            // we don't want to immediately await the echoed close frame as the peer may elect to
            // drain any pending messages **before** echoing the close frame

            let write_result = framed
                .write(
                    OpCode::ControlCode(ControlCode::Close),
                    HeaderFlags::FIN,
                    &mut code,
                    |_, _| Ok(()),
                )
                .await;
            match write_result {
                Ok(()) => *close_state = CloseState::Closing,
                Err(_) => {
                    if server {
                        // 7.1.1: the TCP stream should be closed first by the server
                        //
                        // We aren't interested in any IO errors produced here as the peer *may* have
                        // already closed the TCP stream.
                        framed.close().await;
                    }
                    *close_state = CloseState::Closed;
                }
            }
            match ret {
                Some(err) => Err(err),
                None => Ok(Message::Close(reason)),
            }
        }
        CloseState::Closing => {
            *close_state = CloseState::Closed;
            if server {
                // 7.1.1: the TCP stream should be closed first by the server
                //
                // We aren't interested in any IO errors produced here as the peer *may* have
                // already closed the TCP stream.
                framed.close().await;
            }

            Err(ret.unwrap_or_else(|| Error::with_cause(ErrorKind::Close, CloseCause::Stopped)))
        }
        CloseState::Closed => Err(Error::with_cause(ErrorKind::Close, CloseCause::Error)),
    }
}

pub fn extension_encode<E>(
    extension: &mut E,
    buf: &mut BytesMut,
    header: &mut ExtFrameHeader,
) -> Result<(), Error>
where
    E: ExtensionEncoder,
{
    extension
        .encode(buf, header)
        .map_err(|e| Error::with_cause(ErrorKind::Extension, e))
}

#[cfg(test)]
mod tests {
    use crate::framed::Item;
    use crate::protocol::{ControlCode, DataCode, HeaderFlags, OpCode};
    use crate::ws::extension_encode;
    use crate::{
        CloseCause, CloseCode, CloseReason, Error, Message, NegotiatedExtension, NoExt, Role,
        WebSocket, WebSocketConfig, WebSocketStream,
    };
    use bytes::{Bytes, BytesMut};
    use ratchet_ext::Extension;
    use tokio::io::{duplex, DuplexStream};

    impl<S, E> WebSocket<S, E>
    where
        S: WebSocketStream,
        E: Extension,
    {
        pub async fn write_frame<A>(
            &mut self,
            buf: A,
            opcode: OpCode,
            fin: bool,
        ) -> Result<(), Error>
        where
            A: AsRef<[u8]>,
        {
            let WebSocket { framed, .. } = self;
            let encoder = &mut self.extension;

            framed
                .write(
                    opcode,
                    if fin {
                        HeaderFlags::FIN
                    } else {
                        HeaderFlags::empty()
                    },
                    buf,
                    |payload, header| extension_encode(encoder, payload, header),
                )
                .await
        }

        pub async fn read_frame(&mut self, read_buffer: &mut BytesMut) -> Result<Item, Error> {
            let WebSocket {
                framed, extension, ..
            } = self;

            framed.read_next(read_buffer, extension).await
        }
    }

    fn fixture() -> (
        WebSocket<DuplexStream, NoExt>,
        WebSocket<DuplexStream, NoExt>,
    ) {
        let (server, client) = duplex(512);
        let config = WebSocketConfig::default();

        let server = WebSocket::from_upgraded(
            config,
            server,
            NegotiatedExtension::from(NoExt),
            BytesMut::new(),
            Role::Server,
        );
        let client = WebSocket::from_upgraded(
            config,
            client,
            NegotiatedExtension::from(NoExt),
            BytesMut::new(),
            Role::Client,
        );

        (client, server)
    }

    #[tokio::test]
    async fn ping_pong() {
        let (mut client, mut server) = fixture();
        let payload = "ping!";
        client.write_ping(payload).await.expect("Send failed.");

        let mut read_buf = BytesMut::new();
        let message = server.read(&mut read_buf).await.expect("Read failure");

        assert_eq!(message, Message::Ping(Bytes::from("ping!")));
        assert!(read_buf.is_empty());

        let message = client.read(&mut read_buf).await.expect("Read failure");
        assert_eq!(message, Message::Pong(Bytes::from("ping!")));
        assert!(read_buf.is_empty());
    }

    #[tokio::test]
    async fn reads_unsolicited_pong() {
        let (mut client, mut server) = fixture();
        let payload = "pong!";

        let mut read_buf = BytesMut::new();
        server.write_pong(payload).await.expect("Write failure");

        let message = client.read(&mut read_buf).await.expect("Read failure");
        assert_eq!(message, Message::Pong(Bytes::from(payload)));
        assert!(read_buf.is_empty());
    }

    #[tokio::test]
    async fn empty_control_frame() {
        let (mut client, mut server) = fixture();

        let mut read_buf = BytesMut::new();
        server.write_pong(&[]).await.expect("Write failure");

        let message = client.read(&mut read_buf).await.expect("Read failure");
        assert_eq!(message, Message::Pong(Bytes::new()));
        assert!(read_buf.is_empty());
    }

    #[tokio::test]
    async fn interleaved_control_frames() {
        let (mut client, mut server) = fixture();
        let control_data = "data";

        client
            .write_frame("123", OpCode::DataCode(DataCode::Text), false)
            .await
            .expect("Write failure");
        client
            .write_frame("456", OpCode::DataCode(DataCode::Continuation), false)
            .await
            .expect("Write failure");

        client
            .write_frame(control_data, OpCode::ControlCode(ControlCode::Ping), true)
            .await
            .expect("Write failure");

        client
            .write_frame(control_data, OpCode::ControlCode(ControlCode::Pong), true)
            .await
            .expect("Write failure");

        client
            .write_frame("789", OpCode::DataCode(DataCode::Continuation), true)
            .await
            .expect("Write failure");

        let mut buf = BytesMut::new();
        let message = server.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Ping(Bytes::from(control_data)));
        assert!(!buf.is_empty());

        let message = server.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Pong(Bytes::from(control_data)));
        assert!(!buf.is_empty());

        let message = server.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Text);
        assert!(!buf.is_empty());

        assert_eq!(
            String::from_utf8(buf.to_vec()).expect("Malformatted data received"),
            "123456789"
        );
    }

    #[tokio::test]
    async fn large_control_frames() {
        {
            let (mut client, _server) = fixture();
            let error = client.write_ping(&[13; 256]).await.unwrap_err();
            assert!(error.is_protocol());
        }
        {
            let (mut client, mut server) = fixture();
            server
                .write_frame(&[13; 256], OpCode::ControlCode(ControlCode::Pong), true)
                .await
                .expect("Write failure");

            let error = client.read(&mut BytesMut::new()).await.unwrap_err();
            assert!(error.is_protocol());
        }
    }

    #[tokio::test]
    async fn closes_cleanly() {
        let (mut client, mut server) = fixture();

        let reason = CloseReason::new(CloseCode::GoingAway, Some("Bonsoir, Elliot".to_string()));

        client.close(reason.clone()).await.expect("Close failure");

        drop(client);

        let mut buf = BytesMut::new();
        let message = server.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Close(Some(reason)));
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn drop_end() {
        let (client, mut server) = fixture();
        drop(client);

        let mut buf = BytesMut::new();
        let err = server.read(&mut buf).await.expect_err("Read failure");
        assert!(err.is_io());
    }

    #[tokio::test]
    async fn after_close() {
        let (mut client, mut server) = fixture();
        let reason = CloseReason::new(CloseCode::Normal, None);

        client.close(reason.clone()).await.expect("Close failure");
        client
            .write_ping(&[])
            .await
            .expect_err("Expected a write failure");

        let mut buf = BytesMut::new();
        let message = server.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Close(Some(reason.clone())));
        assert!(buf.is_empty());

        let error = client
            .read(&mut buf)
            .await
            .expect_err("Expected a close error");
        assert!(error.is_close());

        let source = error.downcast_ref::<CloseCause>().unwrap();
        assert_eq!(source, &CloseCause::Stopped);
    }

    #[tokio::test]
    async fn close_code_disagreement() {
        let (mut client, mut server) = fixture();

        client
            .framed
            .write_close(CloseReason::new(CloseCode::Normal, None))
            .await
            .expect("Write failure");
        server
            .framed
            .write_close(CloseReason::new(CloseCode::Protocol, None))
            .await
            .expect("Write failure");

        let mut buf = BytesMut::new();
        let message = client.read(&mut buf).await.expect("Read failure");
        assert_eq!(
            message,
            Message::Close(Some(CloseReason::new(CloseCode::Protocol, None)))
        );

        let message = server.read(&mut buf).await.expect("Read failure");
        assert_eq!(
            message,
            Message::Close(Some(CloseReason::new(CloseCode::Normal, None)))
        );
    }

    #[tokio::test]
    async fn read_before_close() {
        let (mut client, mut server) = fixture();
        let reason = CloseReason::new(CloseCode::Normal, Some("reason".to_string()));

        server.close(reason.clone()).await.expect("Write failure");

        for i in 0..5 {
            client
                .write_text(i.to_string())
                .await
                .expect("Write failure");
        }

        let mut buf = BytesMut::new();
        let message = client.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Close(Some(reason)));

        let mut buf = BytesMut::new();

        for _ in 0..5 {
            let message = server.read(&mut buf).await.expect("Read failure");
            assert_eq!(message, Message::Text);
        }

        let err = server
            .read(&mut buf)
            .await
            .expect_err("Expected a nominal closure");

        assert!(err.is_close())
    }

    #[tokio::test]
    async fn close_then_err() {
        let (mut client, server) = fixture();
        client
            .close(CloseReason::new(CloseCode::Normal, None))
            .await
            .expect("Write error");
        drop(server);

        let mut buf = BytesMut::new();
        client
            .read(&mut buf)
            .await
            .expect_err("Expected a broken connection");
    }

    #[tokio::test]
    async fn reuse_after_closure() {
        let (mut client, mut server) = fixture();
        let reason = CloseReason::new(CloseCode::Normal, None);

        client.close(reason.clone()).await.expect("Write failure");

        let mut buf = BytesMut::new();
        let message = server.read(&mut buf).await.expect("Read failure");

        assert_eq!(message, Message::Close(Some(reason.clone())));
        assert!(buf.is_empty());

        let err = client
            .read(&mut buf)
            .await
            .expect_err("Expected a read failure");
        assert!(err.is_close());
        assert_eq!(
            err.downcast_ref::<CloseCause>().unwrap(),
            &CloseCause::Stopped
        );

        let err = client
            .read(&mut buf)
            .await
            .expect_err("Expected a read failure");
        assert!(err.is_close());
        assert_eq!(
            err.downcast_ref::<CloseCause>().unwrap(),
            &CloseCause::Error
        );
    }
}
