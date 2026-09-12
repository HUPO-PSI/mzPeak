use std::io;

use sha2::{self, Digest};

/// A helper that computes a SHA-512 checksum of a readable stream
pub fn checksum_stream<R: io::Read>(stream: &mut R) -> io::Result<String> {
    let mut context: sha2::Sha512 = sha2::Sha512::new();
    let mut buf = [0u8; 65536];
    loop {
        let z = stream.read(&mut buf)?;
        if z == 0 {
            break;
        }
        context.update(&buf[..z]);
    }
    Ok(hex::encode(context.finalize()))
}


/// A writable stream that keeps a running SHA-512 checksum of all bytes
#[derive(Clone)]
pub struct SHA512HashingStream<T> {
    pub stream: T,
    pub hasher: sha2::Sha512,
}

impl<T> SHA512HashingStream<T> {
    pub fn new(file: T) -> SHA512HashingStream<T> {
        Self {
            stream: file,
            hasher: sha2::Sha512::new(),
        }
    }

    pub fn digest(&self) -> String {
        hex::encode(self.hasher.clone().finalize())
    }

    pub fn hasher(&self) -> &sha2::Sha512 {
        &self.hasher
    }

    pub fn reset_hasher(&mut self) {
        self.hasher = sha2::Sha512::new();
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.stream
    }

    pub fn into_inner(self) -> T {
        self.stream
    }
}

impl<T: io::Write> io::Write for SHA512HashingStream<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl<T: io::Seek + io::Write> io::Seek for SHA512HashingStream<T> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.stream.seek(pos)
    }
}

#[cfg(feature = "async")]
mod async_impl {
    use super::*;

    /// A helper that computes a SHA-512 checksum of a readable asynchronous stream
    pub async fn checksum_stream_async<R: tokio::io::AsyncReadExt + Unpin>(stream: &mut R) -> io::Result<String> {
        let mut context: sha2::Sha512 = sha2::Sha512::new();
        let mut buf = [0u8; 65536];
        loop {
            let z = stream.read(&mut buf).await?;
            if z == 0 {
                break;
            }
            context.update(&buf[..z]);
        }
        Ok(hex::encode(context.finalize()))
    }
}
#[cfg(feature = "async")]
pub use async_impl::checksum_stream_async;