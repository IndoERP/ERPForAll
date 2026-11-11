use bytes::{Buf, Bytes, BytesMut};
use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend;
use postgres_protocol::message::frontend::CopyData;
use std::io;
use tokio_util::codec::{Decoder, Encoder};

pub enum FrontendMessage {
    Raw(Bytes),
    CopyData(CopyData<Box<dyn Buf + Send>>),
}

pub enum BackendMessage {
    Normal {
        messages: BackendMessages,
        request_complete: bool,
    },
    Async(backend::Message),
}

pub struct BackendMessages(BytesMut);

impl BackendMessages {
    pub fn empty() -> BackendMessages {
        BackendMessages(BytesMut::new())
    }
}

impl FallibleIterator for BackendMessages {
    type Item = backend::Message;
    type Error = io::Error;

    fn next(&mut self) -> io::Result<Option<backend::Message>> {
        // A batch holds whole messages only (`PostgresCodec::decode` splits it off at a message
        // boundary), so an empty batch is the end. `Message::parse` would reserve room for the
        // next header first: on a batch that shares its storage with the read buffer, that is a
        // new buffer of the read buffer's original capacity (8 KiB), allocated and dropped once
        // per batch for nothing (amendment A3).
        if self.0.is_empty() {
            return Ok(None);
        }
        backend::Message::parse(&mut self.0)
    }
}

pub struct PostgresCodec;

impl Encoder<FrontendMessage> for PostgresCodec {
    type Error = io::Error;

    fn encode(&mut self, item: FrontendMessage, dst: &mut BytesMut) -> io::Result<()> {
        match item {
            FrontendMessage::Raw(buf) => dst.extend_from_slice(&buf),
            FrontendMessage::CopyData(data) => data.write(dst),
        }

        Ok(())
    }
}

impl Decoder for PostgresCodec {
    type Item = BackendMessage;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<BackendMessage>, io::Error> {
        let mut idx = 0;
        let mut request_complete = false;

        while let Some(header) = backend::Header::parse(&src[idx..])? {
            let len = header.len() as usize + 1;
            if src[idx..].len() < len {
                break;
            }

            match header.tag() {
                backend::NOTICE_RESPONSE_TAG
                | backend::NOTIFICATION_RESPONSE_TAG
                | backend::PARAMETER_STATUS_TAG => {
                    if idx == 0 {
                        let message = backend::Message::parse(src)?.unwrap();
                        return Ok(Some(BackendMessage::Async(message)));
                    } else {
                        break;
                    }
                }
                _ => {}
            }

            idx += len;

            if header.tag() == backend::READY_FOR_QUERY_TAG {
                request_complete = true;
                break;
            }
        }

        if idx == 0 {
            Ok(None)
        } else {
            Ok(Some(BackendMessage::Normal {
                messages: BackendMessages(src.split_to(idx)),
                request_complete,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `ParseComplete` message: tag `1`, length 4.
    const PARSE_COMPLETE: &[u8] = b"1\0\0\0\x04";

    #[test]
    fn an_exhausted_batch_reserves_nothing() {
        let mut read = BytesMut::with_capacity(8192);
        read.extend_from_slice(PARSE_COMPLETE);
        read.extend_from_slice(PARSE_COMPLETE);
        // the codec splits the batch off the read buffer, which keeps the rest of the storage
        let mut batch = BackendMessages(read.split_to(2 * PARSE_COMPLETE.len()));
        assert!(matches!(
            batch.next(),
            Ok(Some(backend::Message::ParseComplete))
        ));
        assert!(matches!(
            batch.next(),
            Ok(Some(backend::Message::ParseComplete))
        ));
        assert!(matches!(batch.next(), Ok(None)));
        assert!(matches!(batch.next(), Ok(None)));
        // `Message::parse` on the empty batch reserved a fresh 8 KiB buffer here
        assert_eq!(batch.0.capacity(), 0);
        assert_eq!(read.capacity(), 8192 - 2 * PARSE_COMPLETE.len());
    }

    #[test]
    fn an_empty_batch_reserves_nothing() {
        let mut batch = BackendMessages::empty();
        assert!(matches!(batch.next(), Ok(None)));
        assert_eq!(batch.0.capacity(), 0);
    }
}
