use std::io::{BufRead, Write};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::protocol::{MAX_EVENT, MAX_FRAME, Request, Response};

pub type Incoming = mpsc::Receiver<Result<Request, &'static str>>;

pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, &'static str> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().map_err(|_| "transport")?;
        if chunk.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err("invalid_frame")
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(chunk.len());
        if frame.len() + length > MAX_FRAME {
            return Err("invalid_frame");
        }
        frame.extend_from_slice(&chunk[..length]);
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

pub fn stdin() -> Incoming {
    let (sender, receiver) = mpsc::channel(1);
    // Tokio stdin uses an uncancellable blocking read. A dedicated OS thread lets
    // runtime shutdown finish after SIGTERM even if the parent keeps stdin open.
    std::thread::spawn(move || {
        let input = std::io::stdin();
        let mut input = input.lock();
        loop {
            let frame = match read_frame(&mut input) {
                Ok(None) => return,
                Ok(Some(bytes)) => sonic_rs::from_slice::<Request>(&bytes)
                    .map_err(|_| "invalid_frame")
                    .and_then(|request| {
                        request.validate()?;
                        Ok(request)
                    }),
                Err(error) => Err(error),
            };
            let failed = frame.is_err();
            if sender.blocking_send(frame).is_err() || failed {
                return;
            }
        }
    });
    receiver
}

struct WriteFrame {
    bytes: Vec<u8>,
    done: oneshot::Sender<Result<(), ()>>,
}

pub struct Output {
    sender: mpsc::Sender<WriteFrame>,
}

impl Output {
    pub fn stdout() -> Self {
        let (sender, mut receiver) = mpsc::channel::<WriteFrame>(1);
        std::thread::spawn(move || {
            let output = std::io::stdout();
            let mut output = output.lock();
            while let Some(frame) = receiver.blocking_recv() {
                let result = output
                    .write_all(&frame.bytes)
                    .and_then(|_| output.flush())
                    .map_err(|_| ());
                let failed = result.is_err();
                let _ = frame.done.send(result);
                if failed {
                    return;
                }
            }
        });
        Self { sender }
    }

    pub async fn send(&self, frame: &Response<'_>) -> Result<(), &'static str> {
        let mut bytes = sonic_rs::to_vec(frame).map_err(|_| "transport")?;
        let limit = if matches!(frame, Response::Event { .. }) {
            MAX_EVENT
        } else {
            MAX_FRAME
        };
        if bytes.len() > limit {
            return Err("transport");
        }
        bytes.push(b'\n');
        let (done, receipt) = oneshot::channel();
        let write = async {
            self.sender
                .send(WriteFrame { bytes, done })
                .await
                .map_err(|_| "transport")?;
            receipt
                .await
                .map_err(|_| "transport")?
                .map_err(|_| "transport")
        };
        tokio::time::timeout(Duration::from_secs(5), write)
            .await
            .map_err(|_| "transport")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_requires_complete_bounded_lines() {
        let mut input = Cursor::new(b"{}\n[]\n");
        assert_eq!(read_frame(&mut input), Ok(Some(b"{}".to_vec())));
        assert_eq!(read_frame(&mut input), Ok(Some(b"[]".to_vec())));
        assert_eq!(read_frame(&mut input), Ok(None));
        assert_eq!(read_frame(&mut Cursor::new(b"{")), Err("invalid_frame"));
        assert_eq!(
            read_frame(&mut Cursor::new(vec![b'x'; MAX_FRAME + 1])),
            Err("invalid_frame")
        );
        let mut exact = vec![b'x'; MAX_FRAME];
        exact.push(b'\n');
        assert_eq!(
            read_frame(&mut Cursor::new(exact)).unwrap().unwrap().len(),
            MAX_FRAME
        );
    }
}
