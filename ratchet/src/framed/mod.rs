use crate::protocol::{
    apply_mask, CloseCode, CloseCodeParseErr, CloseReason, ControlCode, DataCode, FrameHeader,
    HeaderFlags, OpCode, OpCodeParseErr, Role,
};
use crate::WebSocketStream;
use bytes::{BufMut, BytesMut};
use std::str::Utf8Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::errors::{Error, ErrorKind, ProtocolError};
use bytes::Buf;
use either::Either;
use nanorand::{WyRand, RNG};
use std::convert::TryFrom;

pub const CONTROL_FRAME_LEN: &str = "Control frame length greater than 125";
pub(crate) const CONST_STARTED: &str = "Continuation already started";
pub(crate) const CONST_NOT_STARTED: &str = "Continuation not started";
pub(crate) const ILLEGAL_CLOSE_CODE: &str = "Received a reserved close code";
const U16_MAX: usize = u16::MAX as usize;

#[derive(Debug, PartialEq)]
pub enum Item {
    Binary,
    Text,
    Ping(BytesMut),
    Pong(BytesMut),
    Close((Option<CloseReason>, BytesMut)),
}

bitflags::bitflags! {
    pub struct CodecFlags: u8 {
        const R_CONT    = 0b0000_0001;
        const W_CONT    = 0b0000_0010;
        // If high 'text' else 'binary
        const CONT_TYPE = 0b0000_1000;

        // If high then `server` else `client
        const ROLE     = 0b0000_0100;

        // Below are the reserved bits used by the negotiated extensions
        const RSV1      = 0b0100_0000;
        const RSV2      = 0b0010_0000;
        const RSV3      = 0b0001_0000;
        const RESERVED  = Self::RSV1.bits | Self::RSV2.bits | Self::RSV3.bits;
    }
}

#[derive(Debug)]
pub struct ReadError {
    pub close_with: Option<CloseReason>,
    pub error: Error,
}

impl ReadError {
    fn with<E>(msg: &'static str, source: E) -> ReadError
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        ReadError {
            close_with: Some(CloseReason {
                code: CloseCode::Protocol,
                description: Some(msg.to_string()),
            }),
            error: Error::with_cause(ErrorKind::Protocol, source),
        }
    }
}

macro_rules! read_err_from {
    ($from:ident) => {
        impl From<$from> for ReadError {
            fn from(e: $from) -> Self {
                let cause = format!("{}", e);
                ReadError {
                    close_with: Some(CloseReason {
                        code: CloseCode::Protocol,
                        description: Some(cause.clone()),
                    }),
                    error: Error::with_cause(ErrorKind::Protocol, cause),
                }
            }
        }
    };
}

read_err_from!(OpCodeParseErr);
read_err_from!(CloseCodeParseErr);
read_err_from!(ProtocolError);
read_err_from!(Utf8Error);

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        let cause = format!("{}", e);
        ReadError {
            close_with: None,
            error: Error::with_cause(ErrorKind::Protocol, cause),
        }
    }
}

pub enum FrameDecoder {
    DecodingHeader,
    DecodingPayload(FrameHeader, usize, usize),
}

impl Default for FrameDecoder {
    fn default() -> Self {
        FrameDecoder::DecodingHeader
    }
}

pub enum DecodeResult {
    Incomplete(usize),
    Finished(FrameHeader, BytesMut),
}

impl FrameDecoder {
    pub fn decode(
        &mut self,
        buf: &mut BytesMut,
        is_server: bool,
        rsv_bits: u8,
        max_size: usize,
    ) -> Result<DecodeResult, ReadError> {
        loop {
            match self {
                FrameDecoder::DecodingHeader => {
                    match FrameHeader::read_from(buf, is_server, rsv_bits, max_size)? {
                        Either::Left((header, header_len, payload_len)) => {
                            *self = FrameDecoder::DecodingPayload(header, header_len, payload_len);
                        }
                        Either::Right(count) => return Ok(DecodeResult::Incomplete(count)),
                    }
                }
                FrameDecoder::DecodingPayload(header, header_len, payload_len) => {
                    let frame_len = *header_len + *payload_len;
                    let buf_len = buf.len();

                    if buf_len < frame_len {
                        let dif = frame_len - buf_len;
                        return Ok(DecodeResult::Incomplete(dif));
                    }

                    buf.advance(*header_len);

                    let mut payload = buf.split_to(*payload_len);
                    if let Some(mask) = header.mask {
                        apply_mask(mask, &mut payload);
                    }

                    let result = DecodeResult::Finished(*header, payload);
                    *self = FrameDecoder::DecodingHeader;

                    return Ok(result);
                }
            }
        }
    }
}

struct FramedRead {
    read_buffer: BytesMut,
    decoder: FrameDecoder,
}

impl FramedRead {
    fn new(read_buffer: BytesMut) -> FramedRead {
        FramedRead {
            read_buffer,
            decoder: FrameDecoder::default(),
        }
    }

    async fn read_frame<I>(
        &mut self,
        io: &mut I,
        is_server: bool,
        rsv_bits: u8,
        max_size: usize,
    ) -> Result<(FrameHeader, BytesMut), ReadError>
    where
        I: AsyncRead + Unpin,
    {
        let FramedRead {
            read_buffer,
            decoder,
        } = self;

        loop {
            match decoder.decode(read_buffer, is_server, rsv_bits, max_size)? {
                DecodeResult::Incomplete(count) => {
                    let len = read_buffer.len();
                    read_buffer.resize(len + count, 0u8);
                    io.read_exact(&mut read_buffer[len..]).await?;
                }
                DecodeResult::Finished(header, payload) => return Ok((header, payload)),
            }
        }
    }
}

#[derive(Default)]
struct FramedWrite {
    write_buffer: BytesMut,
    encoder: FrameEncoder,
}

impl FramedWrite {
    async fn write<I, A>(
        &mut self,
        io: &mut I,
        flags: &CodecFlags,
        opcode: OpCode,
        header_flags: HeaderFlags,
        mut payload_ref: A,
    ) -> Result<(), std::io::Error>
    where
        I: AsyncWrite + Unpin,
        A: AsMut<[u8]>,
    {
        let FramedWrite {
            write_buffer,
            encoder,
        } = self;

        let payload = payload_ref.as_mut();
        encoder.encode_frame(write_buffer, flags, opcode, header_flags, payload);

        io.write_all(write_buffer).await?;
        write_buffer.clear();

        io.write_all(payload).await
    }
}

#[derive(Default)]
struct FrameEncoder {
    rand: WyRand,
}

impl FrameEncoder {
    fn encode_frame(
        &mut self,
        write_buffer: &mut BytesMut,
        flags: &CodecFlags,
        opcode: OpCode,
        header_flags: HeaderFlags,
        payload: &mut [u8],
    ) {
        let payload_len = payload.len();

        let mask = if flags.contains(CodecFlags::ROLE) {
            None
        } else {
            let mask = self.rand.generate();
            apply_mask(mask, payload);
            Some(mask)
        };

        let masked = mask.is_some();
        let (second, mut offset) = if masked { (0x80, 6) } else { (0x0, 2) };

        if payload_len >= U16_MAX {
            offset += 8;
        } else if payload_len > 125 {
            offset += 2;
        }

        let additional = if masked { payload_len + offset } else { offset };

        write_buffer.reserve(additional);
        let first = header_flags.bits() | u8::from(opcode);

        if payload_len < 126 {
            write_buffer.extend_from_slice(&[first, second | payload_len as u8]);
        } else if payload_len <= U16_MAX {
            write_buffer.extend_from_slice(&[first, second | 126]);
            write_buffer.put_u16(payload_len as u16);
        } else {
            write_buffer.extend_from_slice(&[first, second | 127]);
            write_buffer.put_u64(payload_len as u64);
        };

        if let Some(mask) = mask {
            write_buffer.put_u32(mask);
        }
    }
}

pub struct FramedIo<I> {
    io: I,
    reader: FramedRead,
    writer: FramedWrite,
    flags: CodecFlags,
    max_size: usize,
}

impl<I> FramedIo<I>
where
    I: WebSocketStream,
{
    pub fn new(io: I, read_buffer: BytesMut, role: Role, max_size: usize) -> Self {
        let role_flag = match role {
            Role::Client => CodecFlags::empty(),
            Role::Server => CodecFlags::ROLE,
        };
        let flags = CodecFlags::from(role_flag);

        FramedIo {
            io,
            reader: FramedRead::new(read_buffer),
            writer: FramedWrite::default(),
            flags,
            max_size,
        }
    }

    pub async fn write<A>(
        &mut self,
        opcode: OpCode,
        header_flags: HeaderFlags,
        payload_ref: A,
    ) -> Result<(), std::io::Error>
    where
        A: AsMut<[u8]>,
    {
        let FramedIo {
            io, writer, flags, ..
        } = self;

        writer
            .write(io, flags, opcode, header_flags, payload_ref)
            .await
    }

    pub(crate) async fn read_next(&mut self, read_into: &mut BytesMut) -> Result<Item, ReadError> {
        let FramedIo {
            io,
            reader,
            flags,
            max_size,
            ..
        } = self;

        let rsv_bits = !flags.bits() & 0x70;
        let is_server = flags.contains(CodecFlags::ROLE);

        loop {
            let (header, payload) = reader
                .read_frame(io, is_server, rsv_bits, *max_size)
                .await?;

            match header.opcode {
                OpCode::DataCode(data_code) => {
                    read_into.put(payload);

                    match data_code {
                        DataCode::Continuation => {
                            if header.flags.contains(HeaderFlags::FIN) {
                                let item = if self.flags.contains(CodecFlags::R_CONT) {
                                    if self.flags.contains(CodecFlags::CONT_TYPE) {
                                        Item::Text
                                    } else {
                                        Item::Binary
                                    }
                                } else {
                                    return Err(ReadError::with(
                                        CONST_NOT_STARTED,
                                        ProtocolError::ContinuationNotStarted,
                                    ));
                                };

                                self.flags
                                    .remove(CodecFlags::R_CONT | CodecFlags::CONT_TYPE);
                                return Ok(item);
                            } else {
                                if self.flags.contains(CodecFlags::R_CONT) {
                                    continue;
                                } else {
                                    return Err(ReadError::with(
                                        CONST_NOT_STARTED,
                                        ProtocolError::FrameOverflow,
                                    ));
                                }
                            }
                        }
                        DataCode::Text => {
                            if self.flags.contains(CodecFlags::R_CONT) {
                                return Err(ReadError::with(
                                    CONST_STARTED,
                                    ProtocolError::ContinuationAlreadyStarted,
                                ));
                            } else if header.flags.contains(HeaderFlags::FIN) {
                                return Ok(Item::Text);
                            } else {
                                if self.flags.contains(CodecFlags::R_CONT) {
                                    return Err(ReadError::with(
                                        CONST_STARTED,
                                        ProtocolError::FrameOverflow,
                                    ));
                                } else {
                                    self.flags
                                        .insert(CodecFlags::R_CONT | CodecFlags::CONT_TYPE);
                                    continue;
                                }
                            }
                        }
                        DataCode::Binary => {
                            if self.flags.contains(CodecFlags::R_CONT) {
                                return Err(ReadError::with(
                                    CONST_STARTED,
                                    ProtocolError::ContinuationAlreadyStarted,
                                ));
                            } else if header.flags.contains(HeaderFlags::FIN) {
                                return Ok(Item::Binary);
                            } else {
                                if self.flags.contains(CodecFlags::R_CONT) {
                                    return Err(ReadError::with(
                                        CONTROL_FRAME_LEN,
                                        ProtocolError::FrameOverflow,
                                    ));
                                } else {
                                    debug_assert!(!self.flags.contains(CodecFlags::CONT_TYPE));
                                    self.flags.insert(CodecFlags::R_CONT);
                                    continue;
                                }
                            }
                        }
                    }
                }
                OpCode::ControlCode(c) => {
                    return match c {
                        ControlCode::Close => {
                            let reason = if payload.len() < 2 {
                                // todo this isn't very efficient
                                (None, BytesMut::new())
                            } else {
                                match CloseCode::try_from([payload[0], payload[1]])? {
                                    close_code if close_code.is_illegal() => {
                                        return Err(ReadError::with(
                                            ILLEGAL_CLOSE_CODE,
                                            ProtocolError::CloseCode(u16::from(close_code)),
                                        ))
                                    }
                                    close_code => {
                                        let close_reason =
                                            std::str::from_utf8(&payload[2..])?.to_string();
                                        let description = if close_reason.is_empty() {
                                            None
                                        } else {
                                            Some(close_reason)
                                        };

                                        let reason = CloseReason::new(close_code, description);

                                        (Some(reason), payload)
                                    }
                                }
                            };

                            Ok(Item::Close(reason))
                        }
                        ControlCode::Ping => {
                            if payload.len() > 125 {
                                return Err(ReadError::with(
                                    CONTROL_FRAME_LEN,
                                    ProtocolError::FrameOverflow,
                                ));
                            } else {
                                Ok(Item::Ping(payload))
                            }
                        }
                        ControlCode::Pong => {
                            if payload.len() > 125 {
                                return Err(ReadError::with(
                                    CONTROL_FRAME_LEN,
                                    ProtocolError::FrameOverflow,
                                ));
                            } else {
                                Ok(Item::Pong(payload))
                            }
                        }
                    };
                }
            }
        }
    }

    pub async fn write_close(&mut self, reason: CloseReason) -> Result<(), Error> {
        let CloseReason { code, description } = reason;
        let mut payload = u16::from(code).to_be_bytes().to_vec();

        if let Some(description) = description {
            payload.extend_from_slice(description.as_bytes());
        }

        self.write(
            OpCode::ControlCode(ControlCode::Close),
            HeaderFlags::FIN,
            payload,
        )
        .await
        .map_err(|e| Error::with_cause(ErrorKind::Close, e))
    }
}