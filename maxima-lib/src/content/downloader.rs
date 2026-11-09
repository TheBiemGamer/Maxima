use std::{
    cmp,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    pin::Pin,
    prelude,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task,
};

use crate::{
    content::{
        manager::DownloaderError,
        zip::{CompressionType, ZipFile, ZipFileEntry},
        zlib::{restore_zlib_state, write_zlib_state},
    },
    util::{
        hash::hash_file_crc32,
        native::{maxima_dir, NativeError, SafeParent, SafeStr},
    },
};
use async_compression::tokio::write::DeflateDecoder;
use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use derive_getters::Getters;
use flate2::bufread::DeflateDecoder as BufreadDeflateDecoder;
use futures::{Stream, StreamExt, TryStreamExt};
use log::{debug, error, info, warn};
use reqwest::Client;
use strum_macros::Display;
use thiserror::Error;
use tokio::{
    fs::{create_dir, create_dir_all, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, BufReader, BufWriter},
    runtime::Handle,
};
use tokio_util::compat::FuturesAsyncReadCompatExt;

fn zstate_path(id: &str, path: &str) -> Result<PathBuf, DownloaderError> {
    let mut path = maxima_dir()?.join("temp/downloader").join(id).join(path);
    path.set_extension("eazstate");
    std::fs::create_dir_all(path.safe_parent()?)?;
    Ok(path)
}

#[async_trait]
trait DownloadDecoder: Send {
    fn save_state(&mut self, buf: &mut BytesMut);
    fn restore_state(&mut self, buf: &mut Bytes);

    fn seek(&mut self, pos: SeekFrom) -> Result<(), DownloaderError>;

    fn write_in_pos(&self) -> u64;
    fn write_out_pos(&self) -> u64;

    fn get_mut<'b>(&mut self) -> Arc<Mutex<dyn AsyncWriteWrapper>>;
}

struct ZLibDeflateDecoder {
    decoder: Arc<Mutex<DeflateDecoder<BufWriter<File>>>>,
}

impl ZLibDeflateDecoder {
    fn new(writer: BufWriter<File>) -> Self {
        Self {
            decoder: Arc::new(Mutex::new(DeflateDecoder::new(writer))),
        }
    }
}

#[async_trait]
impl DownloadDecoder for ZLibDeflateDecoder {
    fn save_state(&mut self, buf: &mut BytesMut) {
        let mut decoder = self.decoder.lock().unwrap();
        let zstream = decoder.inner_mut().decoder_mut().inner.decompress.get_raw();
        write_zlib_state(buf, zstream);
    }

    fn restore_state(&mut self, buf: &mut Bytes) {
        let mut decoder = self.decoder.lock().unwrap();
        let decompress = &mut decoder.inner_mut().decoder_mut().inner.decompress;
        decompress.reset(false);
        let zstream = decompress.get_raw();
        restore_zlib_state(buf, zstream);
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<(), DownloaderError> {
        let mut decoder = self.decoder.lock().unwrap();
        let file = decoder.get_mut();

        let handle = Handle::current();
        let _ = handle.enter();
        futures::executor::block_on(file.seek(pos))?;

        Ok(())
    }

    fn write_in_pos(&self) -> u64 {
        let mut decoder = self.decoder.lock().unwrap();
        let decompress = &mut decoder.inner_mut().decoder_mut().inner.decompress;
        let zstream = decompress.get_raw();
        zstream.total_in as u64
    }

    fn write_out_pos(&self) -> u64 {
        let mut decoder = self.decoder.lock().unwrap();
        let decompress = &mut decoder.inner_mut().decoder_mut().inner.decompress;
        let zstream = decompress.get_raw();
        zstream.total_out as u64
    }

    fn get_mut(&mut self) -> Arc<Mutex<dyn AsyncWriteWrapper>> {
        self.decoder.clone()
    }
}

struct NoopDecoder {
    writer: Arc<Mutex<BufWriter<File>>>,
    pos: u64,
}

impl NoopDecoder {
    pub fn new(writer: BufWriter<File>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            pos: 0,
        }
    }
}

#[async_trait]
impl DownloadDecoder for NoopDecoder {
    fn save_state(&mut self, buf: &mut BytesMut) {
        self.pos = self.writer.lock().unwrap().buffer().len() as u64;
        buf.put_u64(self.pos);
    }

    fn restore_state(&mut self, buf: &mut Bytes) {
        self.pos = buf.get_u64();
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<(), DownloaderError> {
        let mut file = self.writer.lock().unwrap();

        let handle = Handle::current();
        let _ = handle.enter();
        futures::executor::block_on(file.seek(pos))?;

        Ok(())
    }

    fn write_in_pos(&self) -> u64 {
        self.pos
    }

    fn write_out_pos(&self) -> u64 {
        self.pos
    }

    fn get_mut<'b>(&mut self) -> Arc<Mutex<dyn AsyncWriteWrapper>> {
        self.writer.clone()
    }
}

trait AsyncWriteWrapper: AsyncWrite + Unpin + Send {}
impl<T: AsyncWrite + Unpin + Send> AsyncWriteWrapper for T {}

struct AsyncWriterWrapper<'a> {
    id: String,
    path: String,
    zlib_state_file: std::fs::File,
    decoder: &'a mut Box<dyn DownloadDecoder>,
    inner: Arc<Mutex<dyn AsyncWriteWrapper>>,
}

impl<'a> AsyncWriterWrapper<'a> {
    async fn new(
        id: String,
        path: String,
        decoder: &'a mut Box<dyn DownloadDecoder>,
    ) -> Result<Self, DownloaderError> {
        let inner = decoder.get_mut();
        Ok(AsyncWriterWrapper {
            id: id.to_owned(),
            path: path.to_owned(),
            zlib_state_file: std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(zstate_path(&id, &path)?)?,
            decoder,
            inner,
        })
    }
}

impl<'a> AsyncWrite for AsyncWriterWrapper<'a> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> task::Poll<prelude::v1::Result<usize, io::Error>> {
        let poll_result = {
            let mut binding = self.inner.lock().unwrap();
            let inner = Pin::new(&mut *binding);
            inner.poll_write(cx, buf)
        };

        // State serialization is disabled for now.
        // let mut bytes = BytesMut::new();
        // self.decoder.save_state(&mut bytes);

        // self.zlib_state_file.seek(SeekFrom::Start(0))?;
        // self.zlib_state_file.write(&bytes)?;

        poll_result
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<prelude::v1::Result<(), io::Error>> {
        Pin::new(&mut *self.inner.lock().unwrap()).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<prelude::v1::Result<(), io::Error>> {
        Pin::new(&mut *self.inner.lock().unwrap()).poll_shutdown(cx)
    }
}

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("download failed ({0} bytes")]
    DownloadFailed(usize),
    #[error("failed to download chunk `{entry}`: {error}")]
    ChunkDownload {
        entry: String,
        error: reqwest::Error,
    },
    #[error("failed to copy chunk `{entry}`: {error}")]
    ChunkCopy {
        entry: String,
        error: std::io::Error,
    },
}

#[derive(PartialEq, Debug)]
enum EntryDownloadState {
    Fresh,
    Resumable,
    Complete,
    Borked,
}

struct DownloadContext {
    id: String,
    path: PathBuf,
}

type BytesDownloadedCallback = Box<dyn Fn(usize) + Send + Sync>;

const DOWNLOAD_TRIES: usize = 5;

fn decoder_for(file: File, compression: &CompressionType) -> Box<dyn DownloadDecoder> {
    let writer = BufWriter::new(file);
    match compression {
        CompressionType::None => Box::new(NoopDecoder::new(writer)),
        CompressionType::Deflate => Box::new(ZLibDeflateDecoder::new(writer)),
    }
}

struct EntryDownloadRequest<'a> {
    context: &'a DownloadContext,
    url: &'a str,
    entry: &'a ZipFileEntry,
    file_path: &'a Path,
    client: Client,
    decoder: Box<dyn DownloadDecoder>,
    callback: Option<BytesDownloadedCallback>,
    /// Bytes of this entry already passed to `callback`, across all attempts.
    /// Every attempt starts at byte 0, so a retry only reports bytes past this mark.
    reported: AtomicUsize,
}

impl<'a> EntryDownloadRequest<'a> {
    pub fn new(
        context: &'a DownloadContext,
        url: &'a str,
        entry: &'a ZipFileEntry,
        file_path: &'a Path,
        client: Client,
        decoder: Box<dyn DownloadDecoder>,
        callback: Option<BytesDownloadedCallback>,
    ) -> Self {
        Self {
            context,
            url,
            entry,
            file_path,
            client,
            decoder,
            callback,
            reported: AtomicUsize::new(0),
        }
    }

    async fn state(
        context: &DownloadContext,
        entry: &ZipFileEntry,
    ) -> Result<EntryDownloadState, DownloaderError> {
        let path = context.path.join(entry.name());

        let file_size = File::open(&path).await?.metadata().await?.len() as i64;

        if file_size == 0 {
            return Ok(EntryDownloadState::Fresh);
        }

        let entry_size = *entry.uncompressed_size();
        let size_match = entry_size == file_size;

        if !size_match {
            warn!("Size mismatch: {}/{}", entry_size, file_size);
            if file_size > entry_size {
                return Ok(EntryDownloadState::Borked);
            }

            return Ok(EntryDownloadState::Borked);
        }

        // We must be calculating the hash incorrectly or something
        // let hash = match hash_file_crc32(&path) {
        //     Ok(hash) => hash,
        //     Err(_) => {
        //         warn!("Failed to retrieve hash for file {}", entry.name());
        //         0
        //     }
        // };

        // let hash_match = *entry.crc32() != hash;
        // if !hash_match {
        //     warn!("Hash mismatch");
        //     return EntryDownloadState::Borked;
        // }

        Ok(EntryDownloadState::Complete)
    }

    async fn download(&mut self) -> Result<(), DownloadError> {
        for attempt in 0..DOWNLOAD_TRIES {
            if attempt > 0 {
                // Each attempt restarts at byte 0, so it needs a decoder and file
                // position that are fresh too, not the half-fed ones of the failed try.
                match OpenOptions::new().write(true).open(self.file_path).await {
                    Ok(file) => {
                        self.decoder = decoder_for(file, self.entry.compression_type());
                    }
                    Err(err) => {
                        error!("Failed to reopen {} for retry: {}", self.entry.name(), err);
                        break;
                    }
                }
            }

            // State serialization is disabled for now.
            //let start = self.decoder.write_in_pos() as i64;

            let start = 0;

            debug!(
                "Downloading {} from {} to {} ({})",
                self.entry.name(),
                start,
                self.entry.compressed_size(),
                self.entry.uncompressed_size()
            );
            let end = *self.entry.compressed_size();

            match self.download_range(start, end).await {
                Ok(()) => return Ok(()),
                Err(err) => warn!(
                    "Download of {} failed (attempt {}/{}): {}",
                    self.entry.name(),
                    attempt + 1,
                    DOWNLOAD_TRIES,
                    err
                ),
            }
        }

        Err(DownloadError::DownloadFailed(
            self.reported.load(Ordering::SeqCst),
        ))
    }

    /// End is not inclusive
    pub async fn download_range(&mut self, start: i64, end: i64) -> Result<(), DownloaderError> {
        let offset = self.entry.data_offset();
        let range = format!("bytes={}-{}", offset + start as i64, offset + end - 1);

        // The URL is signed, so errors drop it before they are logged or returned.
        let data = match self
            .client
            .get(self.url)
            .header("range", range)
            .send()
            .await
            .and_then(|res| res.error_for_status())
        {
            Ok(res) => res,
            Err(err) => {
                let err = err.without_url();
                error!("Failed to download ({}): {}", self.entry.name(), err);
                return Err(DownloaderError::Download(DownloadError::ChunkDownload {
                    entry: self.entry.name().clone(),
                    error: err,
                }));
            }
        };

        let stream = data.bytes_stream();
        let counting_stream =
            ByteCountingStream::new(stream, self.callback.as_ref(), &self.reported);
        let stream = counting_stream.into_async_read();
        let mut stream_reader = BufReader::new(stream.compat());

        // State deserialization is disabled for now.
        // let out_pos = self.decoder.write_out_pos();
        // self.decoder.seek(SeekFrom::Start(out_pos));

        let mut wrapper = AsyncWriterWrapper::new(
            self.context.id.to_owned(),
            self.entry.name().to_owned(),
            &mut self.decoder,
        )
        .await?;

        let result = tokio::io::copy(&mut stream_reader, &mut wrapper).await;
        if let Err(err) = result {
            return Err(DownloaderError::Download(DownloadError::ChunkCopy {
                entry: self.entry.name().clone(),
                error: err,
            }));
        }

        Ok(())
    }
}

#[derive(Getters)]
pub struct ZipDownloader {
    id: String,
    url: String,
    path: PathBuf,
    client: Client,
    manifest: ZipFile,
}

impl ZipDownloader {
    pub async fn new<P: AsRef<Path>>(
        id: &str,
        zip_url: &str,
        path: P,
    ) -> Result<Self, DownloaderError>
    where
        PathBuf: From<P>,
    {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(DownloaderError::PathNotAbsolute(path));
        }

        let manifest = ZipFile::fetch(zip_url).await?;

        Ok(Self {
            id: id.to_owned(),
            url: zip_url.to_owned(),
            path,
            client: Client::builder().build()?,
            manifest,
        })
    }

    pub async fn read_zip_entry_bytes(
        &self,
        entry: &ZipFileEntry,
        length: u64,
    ) -> Result<Bytes, DownloaderError> {
        let offset = entry.data_offset();
        let compressed_size = *entry.compressed_size();

        let range_header = format!("bytes={}-{}", offset, offset + compressed_size - 1);

        let response = self
            .client
            .get(&self.url)
            .header("Range", range_header)
            .send()
            .await?;

        if !response.status().is_success()
            && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        {
            return Err(DownloaderError::Http(response.status()));
        }

        let compressed_data = response.bytes().await?;
        let decompressed_data = match entry.compression_type() {
            CompressionType::None => {
                let entry_size = *entry.uncompressed_size() as u64;
                let available_length = std::cmp::min(length, entry_size);

                if available_length > compressed_data.len() as u64 {
                    return Err(DownloaderError::EntrySize {
                        requested: available_length,
                        entry: compressed_data.len(),
                    });
                }

                Bytes::copy_from_slice(&compressed_data[..available_length as usize])
            }
            CompressionType::Deflate => {
                let mut decoder = BufreadDeflateDecoder::new(Cursor::new(&compressed_data));
                let mut limited_reader = decoder.take(length);
                let mut decompressed_data = Vec::with_capacity(length as usize);
                limited_reader.read_to_end(&mut decompressed_data)?;

                Bytes::from(decompressed_data)
            }
            any => {
                return Err(DownloaderError::CompressionType(any.to_owned()));
            }
        };

        Ok(decompressed_data)
    }

    pub async fn download_single_file(
        &self,
        entry: &ZipFileEntry,
        callback: Option<BytesDownloadedCallback>,
    ) -> Result<usize, DownloaderError> {
        let file_path = self.path.join(entry.name());

        if !file_path.exists() {
            if !file_path.safe_parent()?.exists() {
                create_dir_all(&file_path.safe_parent()?).await?;
            }

            if entry.name().ends_with("/") && !file_path.exists() {
                // This is a folder, create the dir
                debug!("{} is a directory", entry.name());
                create_dir(file_path).await?;
                return Ok(0);
            }
        }

        if *entry.uncompressed_size() == 0 {
            debug!("{} is empty", entry.name());
            return Ok(0);
        }

        let offset = entry.data_offset();
        debug!("Type: {:?}", entry.compression_type());
        debug!("Compressed Size: {}", entry.compressed_size());
        debug!("Offset: {}", offset);

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&file_path)
            .await?;

        let context = DownloadContext {
            id: self.id.to_owned(),
            path: self.path.clone(),
        };

        let state = EntryDownloadRequest::state(&context, entry).await?;
        if state == EntryDownloadState::Complete {
            if let Some(callback) = callback {
                callback(*entry.compressed_size() as usize);
            }
            return Ok(0);
        }

        if state == EntryDownloadState::Borked {
            warn!("Found borked file {}", entry.name());
            file.set_len(*entry.uncompressed_size() as u64).await?;
        }

        let mut decoder = decoder_for(file, entry.compression_type());

        if state == EntryDownloadState::Resumable {
            let state_file = zstate_path(&self.id, &entry.name())?;
            if state_file.exists() {
                let mut buf = Bytes::from(tokio::fs::read(state_file).await?);
                decoder.restore_state(&mut buf);
            } else {
                tokio::fs::create_dir_all(state_file.safe_parent()?).await?;
            }
        }

        let mut request = EntryDownloadRequest::new(
            &context,
            &self.url,
            entry,
            &file_path,
            self.client.clone(),
            decoder,
            callback,
        );

        request.download().await?;
        Ok(0)
    }
}

struct ByteCountingStream<'a, S> {
    inner: S,
    /// Bytes received in this attempt.
    byte_count: usize,
    callback: Option<&'a BytesDownloadedCallback>,
    /// Bytes of the entry already reported, shared by all attempts.
    reported: &'a AtomicUsize,
}

impl<'a, S> ByteCountingStream<'a, S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
{
    fn new(
        inner: S,
        callback: Option<&'a BytesDownloadedCallback>,
        reported: &'a AtomicUsize,
    ) -> Self {
        ByteCountingStream {
            inner,
            byte_count: 0,
            callback,
            reported,
        }
    }

    fn byte_count(&self) -> usize {
        self.byte_count
    }

    /// Reports only the bytes past what earlier attempts already reported,
    /// so a retry never counts the same byte twice.
    fn report(&mut self, len: usize) {
        self.byte_count += len;
        let previous = self.reported.fetch_max(self.byte_count, Ordering::SeqCst);
        let new_bytes = self.byte_count.saturating_sub(previous);
        if new_bytes > 0 {
            if let Some(callback) = &self.callback {
                callback(new_bytes);
            }
        }
    }
}

impl<'a, S> Stream for ByteCountingStream<'a, S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, tokio::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                self.report(chunk.len());
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(err))) => {
                error!("Downloader error: {}", err.without_url());
                std::task::Poll::Ready(Some(Err(futures::io::Error::other(
                    DownloadError::DownloadFailed(self.byte_count),
                ))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retry_failure_is_an_error() {
        let dir = std::env::temp_dir().join("maxima-retry-failure-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("file.bin");
        std::fs::write(&file_path, b"").unwrap();

        let context = DownloadContext {
            id: "test".to_owned(),
            path: dir.clone(),
        };
        let entry = ZipFileEntry::default();
        let file = OpenOptions::new()
            .write(true)
            .open(&file_path)
            .await
            .unwrap();
        let decoder: Box<dyn DownloadDecoder> = Box::new(NoopDecoder::new(BufWriter::new(file)));

        let mut request = EntryDownloadRequest::new(
            &context,
            "http://127.0.0.1:9/file.zip",
            &entry,
            &file_path,
            Client::new(),
            decoder,
            None,
        );

        assert!(request.download().await.is_err());
    }

    #[tokio::test]
    async fn retry_counts_each_byte_once() {
        let total = Arc::new(AtomicUsize::new(0));
        let counter = total.clone();
        let callback: BytesDownloadedCallback = Box::new(move |bytes| {
            counter.fetch_add(bytes, Ordering::SeqCst);
        });
        let reported = AtomicUsize::new(0);

        // A failed attempt that got 7 bytes in, then a full retry of 10 bytes.
        for chunks in [vec![4usize, 3], vec![5, 5]] {
            let stream = futures::stream::iter(
                chunks
                    .into_iter()
                    .map(|len| Ok::<_, reqwest::Error>(Bytes::from(vec![0u8; len]))),
            );
            let _ = ByteCountingStream::new(stream, Some(&callback), &reported)
                .collect::<Vec<_>>()
                .await;
        }

        assert_eq!(total.load(Ordering::SeqCst), 10);
    }
}
