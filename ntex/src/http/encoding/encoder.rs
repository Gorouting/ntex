//! Stream encoder
use std::error::Error;
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_threadpool::{run, BlockingError, CpuFuture};
use brotli2::write::BrotliEncoder;
use bytes::Bytes;
use flate2::write::{GzEncoder, ZlibEncoder};
use futures::ready;

use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::header::{ContentEncoding, HeaderValue, CONTENT_ENCODING};
use crate::http::{ResponseHead, StatusCode};

use super::Writer;

const INPLACE: usize = 1024;

pub struct Encoder<B> {
    eof: bool,
    body: EncoderBody<B>,
    encoder: Option<ContentEncoder>,
    fut: Option<CpuFuture<ContentEncoder, io::Error>>,
}

impl<B: MessageBody> Encoder<B> {
    pub fn response(
        encoding: ContentEncoding,
        head: &mut ResponseHead,
        body: ResponseBody<B>,
    ) -> ResponseBody<Encoder<B>> {
        let can_encode = !(head.headers().contains_key(&CONTENT_ENCODING)
            || head.status == StatusCode::SWITCHING_PROTOCOLS
            || head.status == StatusCode::NO_CONTENT
            || encoding == ContentEncoding::Identity
            || encoding == ContentEncoding::Auto);

        let body = match body {
            ResponseBody::Other(b) => match b {
                Body::None => return ResponseBody::Other(Body::None),
                Body::Empty => return ResponseBody::Other(Body::Empty),
                Body::Bytes(buf) => {
                    if can_encode {
                        EncoderBody::Bytes(buf)
                    } else {
                        return ResponseBody::Other(Body::Bytes(buf));
                    }
                }
                Body::Message(stream) => EncoderBody::BoxedStream(stream),
            },
            ResponseBody::Body(stream) => EncoderBody::Stream(stream),
        };

        if can_encode {
            // Modify response body only if encoder is not None
            if let Some(enc) = ContentEncoder::encoder(encoding) {
                update_head(encoding, head);
                head.no_chunking(false);
                return ResponseBody::Body(Encoder {
                    body,
                    eof: false,
                    fut: None,
                    encoder: Some(enc),
                });
            }
        }
        ResponseBody::Body(Encoder {
            body,
            eof: false,
            fut: None,
            encoder: None,
        })
    }
}

enum EncoderBody<B> {
    Bytes(Bytes),
    Stream(B),
    BoxedStream(Box<dyn MessageBody>),
}

impl<B: MessageBody> MessageBody for Encoder<B> {
    fn size(&self) -> BodySize {
        if self.encoder.is_none() {
            match self.body {
                EncoderBody::Bytes(ref b) => b.size(),
                EncoderBody::Stream(ref b) => b.size(),
                EncoderBody::BoxedStream(ref b) => b.size(),
            }
        } else {
            BodySize::Stream
        }
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        loop {
            if self.eof {
                return Poll::Ready(None);
            }

            if let Some(ref mut fut) = self.fut {
                let mut encoder = match ready!(Pin::new(fut).poll(cx)) {
                    Ok(item) => item,
                    Err(e) => {
                        let e = match e {
                            BlockingError::Error(e) => e,
                            BlockingError::Canceled => {
                                io::Error::new(io::ErrorKind::Other, "Canceled")
                            }
                        };
                        return Poll::Ready(Some(Err(Box::new(e))));
                    }
                };
                let chunk = encoder.take();
                self.encoder = Some(encoder);
                self.fut.take();
                if !chunk.is_empty() {
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }

            let result = match self.body {
                EncoderBody::Bytes(ref mut b) => {
                    if b.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(std::mem::replace(b, Bytes::new()))))
                    }
                }
                EncoderBody::Stream(ref mut b) => b.poll_next_chunk(cx),
                EncoderBody::BoxedStream(ref mut b) => b.poll_next_chunk(cx),
            };
            match result {
                Poll::Ready(Some(Ok(chunk))) => {
                    if let Some(mut encoder) = self.encoder.take() {
                        if chunk.len() < INPLACE {
                            encoder.write(&chunk)?;
                            let chunk = encoder.take();
                            self.encoder = Some(encoder);
                            if !chunk.is_empty() {
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                        } else {
                            self.fut = Some(run(move || {
                                encoder.write(&chunk)?;
                                Ok(encoder)
                            }));
                        }
                    } else {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                }
                Poll::Ready(None) => {
                    if let Some(encoder) = self.encoder.take() {
                        let chunk = encoder.finish()?;
                        if chunk.is_empty() {
                            return Poll::Ready(None);
                        } else {
                            self.eof = true;
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                    } else {
                        return Poll::Ready(None);
                    }
                }
                val => return val,
            }
        }
    }
}

fn update_head(encoding: ContentEncoding, head: &mut ResponseHead) {
    head.headers_mut().insert(
        CONTENT_ENCODING,
        HeaderValue::from_static(encoding.as_str()),
    );
}

enum ContentEncoder {
    Deflate(ZlibEncoder<Writer>),
    Gzip(GzEncoder<Writer>),
    Br(BrotliEncoder<Writer>),
}

impl ContentEncoder {
    fn encoder(encoding: ContentEncoding) -> Option<Self> {
        match encoding {
            ContentEncoding::Deflate => Some(ContentEncoder::Deflate(ZlibEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            ContentEncoding::Gzip => Some(ContentEncoder::Gzip(GzEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),
            ContentEncoding::Br => {
                Some(ContentEncoder::Br(BrotliEncoder::new(Writer::new(), 3)))
            }
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn take(&mut self) -> Bytes {
        match *self {
            ContentEncoder::Br(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Deflate(ref mut encoder) => encoder.get_mut().take(),
            ContentEncoder::Gzip(ref mut encoder) => encoder.get_mut().take(),
        }
    }

    fn finish(self) -> Result<Bytes, io::Error> {
        match self {
            ContentEncoder::Br(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
            ContentEncoder::Gzip(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
            ContentEncoder::Deflate(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<(), io::Error> {
        match *self {
            ContentEncoder::Br(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding br encoding: {}", err);
                    Err(err)
                }
            },
            ContentEncoder::Gzip(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding gzip encoding: {}", err);
                    Err(err)
                }
            },
            ContentEncoder::Deflate(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding deflate encoding: {}", err);
                    Err(err)
                }
            },
        }
    }
}
