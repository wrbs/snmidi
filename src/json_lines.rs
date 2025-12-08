// A cancel safe json lines bidirectional stream
// todo: consider moving out

use std::io::ErrorKind;

use bstr::BStr;
use color_eyre::eyre::{Context, Error, OptionExt};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader,
        ReadHalf, WriteHalf, split,
    },
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{Level, debug, enabled};

// #[derive(Debug, Error)]
// pub enum Error {
//     #[error("IO error")]
//     IO(#[from] std::io::Error),

//     #[error("JSON parse error")]
//     Json(#[from] serde_json::Error),
// }

// #[derive(Debug, Error)]
// pub enum ErrorOrClosed {
//     #[error("Stream closed")]
//     Closed,

//     #[error(transparent)]
//     Other(Error),
// }

#[allow(unused)]
pub trait ReadJson {
    async fn json_read_opt<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error>;

    async fn json_read<T: DeserializeOwned>(&mut self) -> Result<T, Error> {
        self.json_read_opt().await?.ok_or_eyre("Unexpected EOF")
    }
}

// impl<S> ReadJson for S
// where
//     S: AsyncBufRead + Unpin,
// {
//     async fn json_read_opt<T: DeserializeOwned>(&mut self) ->
// Result<Option<T>, Error> {         let mut buf = vec![];
//         self.read_until(b'\n', &mut buf).await?;
//         Ok(serde_json::from_slice(buf.as_slice())?)
//     }
// }

#[allow(unused)]
pub struct ReadJsonSharedBuf<S>
where
    S: AsyncBufRead + Unpin,
{
    stream: S,
    buffer: Vec<u8>,
}

impl<S> ReadJsonSharedBuf<S>
where
    S: AsyncBufRead + Unpin,
{
    #[allow(unused)]
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    #[allow(unused)]
    pub fn with_capacity(stream: S, capacity: usize) -> Self {
        Self {
            stream,
            buffer: Vec::with_capacity(capacity),
        }
    }
}

impl<S> ReadJson for ReadJsonSharedBuf<S>
where
    S: AsyncBufRead + Unpin,
{
    async fn json_read_opt<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        self.buffer.clear();
        self.stream.read_until(b'\n', &mut self.buffer).await?;
        Ok(serde_json::from_slice(self.buffer.as_slice())?)
    }
}

pub struct LineReaderJob<S>
where
    S: AsyncBufRead + Unpin + Send + 'static,
{
    handle: Option<LineReaderHandle<S>>,
}

impl<S> LineReaderJob<S>
where
    S: AsyncBufRead + Unpin + Send + 'static,
{
    pub async fn new(stream: S) -> LineReaderJob<S> {
        let handle = LineReaderHandle::init(stream).await;
        Self {
            handle: Some(handle),
        }
    }

    pub async fn join(self) -> (S, Option<std::io::Error>) {
        match self.handle {
            None => panic!("cannot join, read thread panicked"),
            Some(handle) => handle.join().await,
        }
    }

    pub async fn read_line(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        let Some(LineReaderHandle::Open { rx, join_handle: _ }) = &mut self.handle
        else {
            return Ok(None);
        };

        match rx.recv().await {
            Some(value) => Ok(Some(value)),
            None => {
                let Some(handle) = self.handle.take() else {
                    return Ok(None);
                };
                let (stream, error) = handle.join().await;
                self.handle = Some(LineReaderHandle::Finished(stream));

                if let Some(err) = error {
                    return Err(err);
                }

                Ok(None)
            },
        }
    }
}

impl<S> ReadJson for LineReaderJob<S>
where
    S: AsyncBufRead + Unpin + Send + 'static,
{
    async fn json_read_opt<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        let read = self.read_line().await?;
        if enabled!(Level::DEBUG) {
            let line = read.as_ref().map(|v| BStr::new(v.as_slice()));
            debug!(line = ?&line, "got line from client");
        }
        Ok(read
            .map(|v| {
                serde_json::from_slice(v.as_slice()).wrap_err_with(|| {
                    let line = String::from_utf8_lossy(v.as_slice());
                    format!("Reading line: {line:?}")
                })
            })
            .transpose()?)
    }
}

enum LineReaderHandle<S> {
    Open {
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
        join_handle: JoinHandle<(S, Option<std::io::Error>)>,
    },
    Finished(S),
}

impl<S> LineReaderHandle<S>
where
    S: AsyncBufRead + Unpin + Send + 'static,
{
    async fn init(mut stream: S) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let join_handle = tokio::spawn(async move {
            let result = async {
                loop {
                    let mut buf = Vec::new();

                    if stream.read_until(b'\n', &mut buf).await? == 0 {
                        break;
                    }

                    if let Err(_) = tx.send(buf) {
                        // channel closed
                        break;
                    }
                }

                Ok(())
            }
            .await;

            let error = match result {
                Ok(()) => None,
                Err(err) => Some(err),
            };

            (stream, error)
        });
        LineReaderHandle::Open { rx, join_handle }
    }

    async fn join(self) -> (S, Option<std::io::Error>) {
        match self {
            LineReaderHandle::Open { join_handle, rx: _ } => {
                let (stream, error) = join_handle.await.unwrap();
                (stream, error)
            },
            LineReaderHandle::Finished(stream) => (stream, None),
        }
    }
}

pub struct JsonLines<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    writer: WriteHalf<S>,
    reader: LineReaderJob<BufReader<ReadHalf<S>>>,
}

impl<S> JsonLines<S>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    // Consider with instead to reliably reraise panics
    pub async fn new(stream: S) -> JsonLines<S> {
        let (reader, writer) = split(stream);
        let reader = LineReaderJob::new(BufReader::new(reader)).await;
        JsonLines { writer, reader }
    }

    // Joins the read thread, re-raising panics but ignoring any unhandled
    // messages
    pub async fn join(self) -> (S, Option<Error>) {
        let (reader, error) = self.reader.join().await;
        (
            reader.into_inner().unsplit(self.writer),
            error.map(Error::from),
        )
    }

    pub async fn write<T: Serialize>(&mut self, message: &T) -> Result<(), Error> {
        let mut s = serde_json::to_string(message)?;
        debug!(s, "writing response");

        s.push('\n');

        self.writer.write_all(s.as_bytes()).await.map_err(|err| {
            if err.kind() == ErrorKind::BrokenPipe {
                Error::from(err).wrap_err("Got broken pipe when writing")
            } else {
                err.into()
            }
        })
    }
}

impl<S> ReadJson for JsonLines<S>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    async fn json_read_opt<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        self.reader.json_read_opt().await
    }
}
