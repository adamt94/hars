//! An event stream (`text/event-stream`) read line by line.

use crate::Error;
use bytes::{Buf, Bytes, BytesMut};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, StatusCode};
use tokio::sync::OwnedSemaphorePermit;

/// aiohttp's longest line, newline included (2 × 64 KiB); a longer one
/// is an error.
pub const MAX_LINE: usize = 128 * 1024;

/// One `key: value` line, split at the first `:` (`value` keeps its
/// leading space). A comment (`: hi`) has an empty key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub key: String,
    pub value: String,
}

/// An open stream: its status and headers, then [`EventStream::next_line`].
pub struct EventStream {
    pub status: StatusCode,
    pub headers: HeaderMap,
    body: Incoming,
    buf: BytesMut,
    done: bool,
    _slot: Option<OwnedSemaphorePermit>,
}

impl EventStream {
    pub(crate) fn new(
        status: StatusCode,
        headers: HeaderMap,
        body: Incoming,
        slot: Option<OwnedSemaphorePermit>,
    ) -> Self {
        Self {
            status,
            headers,
            body,
            buf: BytesMut::new(),
            done: false,
            _slot: slot,
        }
    }

    /// The next line with a `:`, stripped of surrounding whitespace as
    /// aiohue does; blank lines, lines without `:` and lines that aren't
    /// UTF-8 (aiohue's strict `decode()` drops them) are skipped. `None`
    /// when the stream ends.
    pub async fn next_line(&mut self) -> Result<Option<Line>, Error> {
        loop {
            while let Some(raw) = self.take_line()? {
                if let Some(line) = parse(&raw) {
                    return Ok(Some(line));
                }
            }
            if self.done {
                // A last line without a newline.
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let rest = self.buf.split().freeze();
                if let Some(line) = parse(&rest) {
                    return Ok(Some(line));
                }
                return Ok(None);
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                Some(Err(e)) => return Err(Error::Request(e.to_string())),
                None => self.done = true,
            }
        }
    }

    /// A complete line from the buffer, if there is one.
    fn take_line(&mut self) -> Result<Option<Bytes>, Error> {
        match self.buf.iter().position(|b| *b == b'\n') {
            Some(i) if i >= MAX_LINE => Err(Error::TooLarge(MAX_LINE)),
            Some(i) => {
                let line = self.buf.split_to(i).freeze();
                self.buf.advance(1);
                Ok(Some(line))
            }
            // Without its newline yet, a line may still be exactly the limit
            // (if the stream ends there).
            None if self.buf.len() > MAX_LINE => Err(Error::TooLarge(MAX_LINE)),
            None => Ok(None),
        }
    }
}

fn parse(raw: &[u8]) -> Option<Line> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    let (key, value) = text.split_once(':')?;
    Some(Line {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}
