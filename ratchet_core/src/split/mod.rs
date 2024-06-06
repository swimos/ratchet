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

use std::fmt::Debug;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use bytes::BytesMut;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, TryFutureExt};
use log::{error, trace};
use tokio::io::AsyncWriteExt;

use bilock::{bilock, BiLock};
use ratchet_ext::{ExtensionDecoder, ExtensionEncoder, ReunitableExtension, SplittableExtension};

use crate::ext::NegotiatedExtension;
use crate::framed::{
    read_next, write_close, write_fragmented, CodecFlags, FramedIoParts, FramedRead, FramedWrite,
    Item,
};
use crate::protocol::{CloseReason, ControlCode, DataCode, HeaderFlags, MessageType, OpCode};
use crate::ws::{extension_encode, CloseState, WebSocketClose, CONTROL_MAX_SIZE};
use crate::{
    framed, CloseCause, CloseCode, Error, ErrorKind, Message, PayloadType, ProtocolError, Role,
    WebSocket, WebSocketStream,
};

mod bilock;
#[cfg(test)]
mod tests;

type ReuniteFailure<S, E> = ReuniteError<
    S,
    <E as SplittableExtension>::SplitEncoder,
    <E as SplittableExtension>::SplitDecoder,
>;

const STATE_OPEN: u8 = 0;
const STATE_CLOSING: u8 = 1;
const STATE_CLOSED: u8 = 2;

/// Splits a WebSocket's parts into send and receive halves. Internally, two BiLocks are used: one
/// over the IO and one on the write half to send any responses to any control frames that are
/// received.
///
/// # Note
/// It is possible to reunite the halves back into a WebSocket if the extension implements
/// `ReunitableExtension`.
pub fn split<S, E>(
    framed: framed::FramedIo<S>,
    control_buffer: BytesMut,
    extension: NegotiatedExtension<E>,
) -> (Sender<S, E::SplitEncoder>, Receiver<S, E::SplitDecoder>)
where
    S: WebSocketStream,
    E: SplittableExtension,
{
    let FramedIoParts {
        io,
        reader,
        writer,
        flags,
        max_message_size,
    } = framed.into_parts();

    let close_state = Arc::new(AtomicU8::new(STATE_OPEN));
    let (read_half, write_half) = bilock(io);
    let (sender_writer, reader_writer) = bilock(WriteHalf {
        control_buffer,
        split_writer: write_half,
        writer,
        is_server: flags.contains(CodecFlags::ROLE),
    });

    let (ext_encoder, ext_decoder) = extension.split();

    let role = if flags.contains(CodecFlags::ROLE) {
        Role::Server
    } else {
        Role::Client
    };

    let sender = Sender {
        role,
        close_state: close_state.clone(),
        split_writer: sender_writer,
        ext_encoder,
    };
    let receiver = Receiver {
        role,
        close_state,
        framed: FramedIo {
            flags,
            max_message_size,
            read_half,
            reader,
            split_writer: reader_writer,
            ext_decoder,
        },
    };

    (sender, receiver)
}

impl<S> WriteHalf<S>
where
    S: WebSocketStream,
{
    async fn write<A, E>(
        &mut self,
        buf_ref: A,
        message_type: PayloadType,
        header_flags: HeaderFlags,
        is_server: bool,
        extension: &mut E,
    ) -> Result<(), Error>
    where
        A: AsRef<[u8]>,
        E: ExtensionEncoder,
    {
        let WriteHalf {
            split_writer,
            writer,
            control_buffer,
            ..
        } = self;
        let buf = buf_ref.as_ref();

        match message_type {
            PayloadType::Text => writer
                .write(
                    split_writer,
                    is_server,
                    OpCode::DataCode(DataCode::Text),
                    header_flags,
                    buf,
                    |payload, header| extension_encode(extension, payload, header),
                )
                .await
                .map_err(Into::into),
            PayloadType::Binary => writer
                .write(
                    split_writer,
                    is_server,
                    OpCode::DataCode(DataCode::Binary),
                    header_flags,
                    buf,
                    |payload, header| extension_encode(extension, payload, header),
                )
                .await
                .map_err(Into::into),
            PayloadType::Ping => {
                if buf.len() > CONTROL_MAX_SIZE {
                    Err(Error::with_cause(
                        ErrorKind::Protocol,
                        ProtocolError::FrameOverflow,
                    ))
                } else {
                    control_buffer.clear();
                    control_buffer.extend_from_slice(buf);

                    writer
                        .write(
                            split_writer,
                            is_server,
                            OpCode::ControlCode(ControlCode::Ping),
                            header_flags,
                            buf,
                            |payload, header| extension_encode(extension, payload, header),
                        )
                        .await
                        .map_err(Into::into)
                }
            }
            PayloadType::Pong => {
                if buf.len() > CONTROL_MAX_SIZE {
                    Err(Error::with_cause(
                        ErrorKind::Protocol,
                        ProtocolError::FrameOverflow,
                    ))
                } else {
                    writer
                        .write(
                            split_writer,
                            is_server,
                            OpCode::ControlCode(ControlCode::Pong),
                            header_flags,
                            buf,
                            |payload, header| extension_encode(extension, payload, header),
                        )
                        .await
                        .map_err(Into::into)
                }
            }
        }
    }
}

#[derive(Debug)]
struct WriteHalf<S> {
    split_writer: BiLock<S>,
    writer: FramedWrite,
    control_buffer: BytesMut,
    is_server: bool,
}

#[derive(Debug)]
struct FramedIo<S, E> {
    flags: CodecFlags,
    max_message_size: usize,
    read_half: BiLock<S>,
    reader: FramedRead,
    split_writer: BiLock<WriteHalf<S>>,
    ext_decoder: NegotiatedExtension<E>,
}

/// An owned write half of a WebSocket connection.
#[derive(Debug)]
pub struct Sender<S, E> {
    role: Role,
    close_state: Arc<AtomicU8>,
    split_writer: BiLock<WriteHalf<S>>,
    ext_encoder: NegotiatedExtension<E>,
}

impl<S, E> Sender<S, E>
where
    S: WebSocketStream,
    E: ExtensionEncoder,
{
    /// Attempt to reunite this send half with its receiver.
    ///
    /// # Errors
    /// Errors if `receiver` is not paired with this sender.
    pub fn reunite<Ext>(
        self,
        receiver: Receiver<S, Ext::SplitDecoder>,
    ) -> Result<WebSocket<S, Ext>, ReuniteFailure<S, Ext>>
    where
        S: Debug,
        Ext: ReunitableExtension<SplitEncoder = E>,
    {
        reunite::<S, Ext>(self, receiver)
    }

    /// Returns the role of this Sender.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Returns whether this WebSocket is closed.
    pub fn is_closed(&self) -> bool {
        self.close_state.load(Ordering::SeqCst) == STATE_CLOSED
    }

    /// Returns whether this WebSocket is closing or closed.
    pub fn is_active(&self) -> bool {
        matches!(self.close_state.load(Ordering::SeqCst), STATE_OPEN)
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

    /// Constructs a new WebSocket message of `message_type` and with a payload of `buf_ref.
    pub async fn write<A>(&mut self, buf: A, message_type: PayloadType) -> Result<(), Error>
    where
        A: AsRef<[u8]>,
    {
        if !self.is_active() {
            return Err(Error::with_cause(ErrorKind::Close, CloseCause::Error));
        }

        let writer = &mut *self.split_writer.lock().await;
        writer
            .write(
                buf,
                message_type,
                HeaderFlags::FIN,
                self.role.is_server(),
                &mut self.ext_encoder,
            )
            .await
    }

    /// Sends a new WebSocket message of `message_type` and with a payload of `buf_ref` and chunked
    /// by `fragment_size`. If the length of the buffer is less than the chunk size then only a
    /// single message is sent.
    pub async fn write_fragmented<A>(
        &mut self,
        buf: A,
        message_type: MessageType,
        fragment_size: usize,
    ) -> Result<(), Error>
    where
        A: AsRef<[u8]>,
    {
        if self.is_closed() {
            return Err(Error::with_cause(ErrorKind::Close, CloseCause::Error));
        }
        let WriteHalf {
            split_writer,
            writer,
            ..
        } = &mut *self.split_writer.lock().await;
        let ext_encoder = &mut self.ext_encoder;
        write_fragmented(
            split_writer,
            writer,
            buf,
            message_type,
            fragment_size,
            self.role.is_server(),
            |payload, header| extension_encode(ext_encoder, payload, header),
        )
        .await
    }

    /// Close this WebSocket with the reason provided.
    ///
    /// If the WebSocket is already closed then `Ok(())` is returned.
    pub async fn close(&mut self, reason: CloseReason) -> Result<(), Error> {
        if !self.is_active() {
            return Ok(());
        }

        self.close_state.store(STATE_CLOSING, Ordering::SeqCst);

        let WriteHalf {
            split_writer,
            writer,
            ..
        } = &mut *self.split_writer.lock().await;
        write_close(split_writer, writer, reason, self.role.is_server()).await
    }
}

/// An owned read half of a WebSocket connection.
#[derive(Debug)]
pub struct Receiver<S, E> {
    role: Role,
    close_state: Arc<AtomicU8>,
    framed: FramedIo<S, E>,
}

impl<S, E> Receiver<S, E>
where
    S: WebSocketStream,
    E: ExtensionDecoder,
{
    /// Returns the role of this Receiver.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Attempt to read some data from the WebSocket. Returning either the type of the message
    /// received or the error that was produced.
    ///
    /// # Errors
    /// If an error is produced during a read operation the contents of `read_buffer` must be
    /// considered to be dirty.
    ///
    /// # Note
    /// Ratchet transparently handles ping messages received from the peer by returning a pong frame
    /// and this function will return `Message::Pong` if one has been received. As per [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455)
    /// these may be interleaved between data frames. In the event of one being received while
    /// reading a continuation, this function will then yield `Message::Ping` and the `read_buffer`
    /// will contain the data received up to that point. The callee must ensure that the contents
    /// of `read_buffer` are **not** then modified before calling `read` again.
    pub async fn read(&mut self, read_buffer: &mut BytesMut) -> Result<Message, Error> {
        if self.is_closed() {
            return Err(Error::with_cause(ErrorKind::Close, CloseCause::Error));
        }

        let Receiver {
            role,
            close_state,
            framed,
            ..
        } = self;
        let FramedIo {
            flags,
            max_message_size,
            read_half,
            reader,
            split_writer,
            ext_decoder,
        } = framed;
        let is_server = role.is_server();

        match read_next(
            read_half,
            reader,
            flags,
            *max_message_size,
            read_buffer,
            ext_decoder,
        )
        .await
        {
            Ok(item) => match item {
                Item::Binary => Ok(Message::Binary),
                Item::Text => Ok(Message::Text),
                Item::Ping(payload) => {
                    trace!("Received a ping frame. Responding with pong");

                    let WriteHalf {
                        split_writer,
                        writer,
                        ..
                    } = &mut *split_writer.lock().await;

                    let ret = payload.clone().freeze();
                    writer
                        .write(
                            split_writer,
                            is_server,
                            OpCode::ControlCode(ControlCode::Pong),
                            HeaderFlags::FIN,
                            payload,
                            |_, _| Ok(()),
                        )
                        .await?;
                    Ok(Message::Ping(ret))
                }
                Item::Pong(payload) => {
                    let WriteHalf { control_buffer, .. } = &mut *split_writer.lock().await;

                    if control_buffer.is_empty() {
                        trace!("Received an unsolicited pong frame");
                    } else {
                        control_buffer.clear();
                        trace!("Received pong frame");
                    }
                    Ok(Message::Pong(payload.freeze()))
                }
                Item::Close(reason) => {
                    let code = reason
                        .as_ref()
                        .map(|reason| reason.code)
                        .unwrap_or(CloseCode::Normal);

                    close(
                        role.is_server(),
                        close_state,
                        &mut *split_writer.lock().await,
                        code,
                    )
                    .await?;
                    Ok(Message::Close(reason))
                }
            },
            Err(e) => {
                error!("WebSocket read failure: {:?}", e);

                // We want to close the connection but return the error produced during the session,
                // not any during the close sequence.
                let _ = close(
                    role.is_server(),
                    close_state,
                    &mut *split_writer.lock().await,
                    CloseCode::Protocol,
                )
                .await;
                Err(e)
            }
        }
    }

    /// Close this WebSocket with the reason provided.
    ///
    /// If the WebSocket is already closed then `Ok(())` is returned.
    pub async fn close(&mut self, reason: CloseReason) -> Result<(), Error> {
        if !self.is_active() {
            return Ok(());
        }

        self.close_state.store(STATE_CLOSING, Ordering::SeqCst);

        let WriteHalf {
            split_writer,
            writer,
            ..
        } = &mut *self.framed.split_writer.lock().await;
        write_close(split_writer, writer, reason, self.role.is_server()).await
    }

    /// Returns whether this WebSocket is closed.
    pub fn is_closed(&self) -> bool {
        self.close_state.load(Ordering::SeqCst) == STATE_CLOSED
    }

    /// Returns whether this WebSocket is closing or closed.
    pub fn is_active(&self) -> bool {
        matches!(self.close_state.load(Ordering::SeqCst), STATE_OPEN)
    }
}

impl<S> WebSocketClose for WriteHalf<S>
where
    S: WebSocketStream,
{
    fn write(&mut self, code: CloseCode) -> BoxFuture<Result<(), Error>> {
        let WriteHalf {
            split_writer,
            writer,
            is_server,
            ..
        } = self;

        Box::pin(async move {
            writer
                .write(
                    split_writer,
                    *is_server,
                    OpCode::ControlCode(ControlCode::Close),
                    HeaderFlags::FIN,
                    &mut u16::from(code).to_be_bytes(),
                    |_, _| Ok(()),
                )
                .await
        })
    }

    fn shutdown(&mut self) -> BoxFuture<Result<(), Error>> {
        self.split_writer.shutdown().map_err(Into::into).boxed()
    }
}

async fn close<S>(
    is_server: bool,
    state_ref: &AtomicU8,
    framed: &mut WriteHalf<S>,
    code: CloseCode,
) -> Result<(), Error>
where
    S: WebSocketStream,
{
    let close_state = match state_ref.load(Ordering::SeqCst) {
        STATE_OPEN => CloseState::NotClosed,
        STATE_CLOSING => CloseState::Closing,
        STATE_CLOSED => CloseState::Closing,
        s => panic!("Unexpected close state: {}", s),
    };
    let close_result = crate::ws::close(framed, is_server, close_state, code).await;

    state_ref.store(STATE_CLOSED, Ordering::SeqCst);
    close_result
}

/// An error produced by `reunite` if the halves do not match.
#[derive(Debug)]
#[allow(missing_docs)]
pub struct ReuniteError<S, E, D> {
    pub sender: Sender<S, E>,
    pub receiver: Receiver<S, D>,
}

/// Attempts to reunites the send and receive halves that form a WebSocket or returns an error if
/// they do not represent the same connection.
fn reunite<S, E>(
    sender: Sender<S, E::SplitEncoder>,
    receiver: Receiver<S, E::SplitDecoder>,
) -> Result<WebSocket<S, E>, ReuniteFailure<S, E>>
where
    S: WebSocketStream + Debug,
    E: ReunitableExtension,
{
    if sender
        .split_writer
        .same_bilock(&receiver.framed.split_writer)
    {
        let Sender {
            split_writer: sender_writer,
            ext_encoder,
            ..
        } = sender;
        let Receiver {
            close_state,
            framed,
            ..
        } = receiver;
        let FramedIo {
            flags,
            max_message_size,
            read_half,
            reader,
            ext_decoder,
            split_writer: reader_writer,
        } = framed;

        let WriteHalf {
            split_writer,
            writer,
            control_buffer,
            ..
        } = sender_writer
            .reunite(reader_writer)
            // This is safe as we have checked the pointers
            .expect("Failed to reunite writer");

        let framed = framed::FramedIo::from_parts(FramedIoParts {
            // This is safe as we have checked the pointers
            io: read_half
                .reunite(split_writer)
                .expect("Failed to reunite IO"),
            reader,
            writer,
            flags,
            max_message_size,
        });

        let close_state = match close_state.load(Ordering::SeqCst) {
            STATE_OPEN => CloseState::NotClosed,
            STATE_CLOSING => CloseState::Closing,
            STATE_CLOSED => CloseState::Closed,
            s => panic!("Unknown close state: {}", s),
        };

        Ok(WebSocket::from_parts(
            framed,
            control_buffer,
            NegotiatedExtension::reunite(ext_encoder, ext_decoder),
            close_state,
        ))
    } else {
        Err(ReuniteError { sender, receiver })
    }
}
