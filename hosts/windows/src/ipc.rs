use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Full-duplex byte stream backed by one inherited anonymous pipe in each direction.
pub struct PipeStream {
    reader: tokio::fs::File,
    writer: tokio::fs::File,
}

impl PipeStream {
    pub fn new(reader: std::fs::File, writer: std::fs::File) -> Self {
        Self {
            reader: tokio::fs::File::from_std(reader),
            writer: tokio::fs::File::from_std(writer),
        }
    }

    #[cfg(windows)]
    pub fn from_inherited_handles(read_handle: isize, write_handle: isize) -> Result<Self, String> {
        use std::os::windows::io::FromRawHandle;

        if read_handle == 0 || read_handle == -1 || write_handle == 0 || write_handle == -1 {
            return Err("session agent received an invalid IPC handle".to_string());
        }
        // SAFETY: CreateProcessAsUser inherited exactly these two uniquely-owned pipe handles into
        // this process. Each value is consumed once and the resulting Files close them on drop.
        let reader = unsafe { std::fs::File::from_raw_handle(read_handle as _) };
        // SAFETY: same ownership contract as the read handle above.
        let writer = unsafe { std::fs::File::from_raw_handle(write_handle as _) };
        Ok(Self::new(reader, writer))
    }
}

impl AsyncRead for PipeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(context, buffer)
    }
}

impl AsyncWrite for PipeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_shutdown(context)
    }
}
