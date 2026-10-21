//! Compressed batch files.
//!
//! A batch file is mostly the same system prompt and schema repeated on every
//! line, so it compresses far better than a single request. A file part that
//! starts with a gzip or zstd magic number is decoded as it streams in, so the
//! JSONL parser, the file size limit and the stored file all see the decoded
//! content. Formats we can recognise but don't decode are named in the error,
//! rather than failing as invalid UTF-8 on line 1.
//!
//! Detection is by content, not by file name or part `Content-Type`: no JSONL
//! file can start with any of these signatures, while names and part types
//! are often missing or wrong.

use std::{io, pin::Pin};

use async_compression::tokio::bufread::{GzipDecoder, ZstdDecoder};
use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt, stream};
use tokio_util::io::{ReaderStream, StreamReader};

/// Long enough for the longest signature below (xz).
const SNIFF_LEN: usize = 6;

/// Size of the decoded chunks handed to the JSONL parser.
const DECODED_CHUNK: usize = 64 * 1024;

/// The largest zstd window a file may need: zstd's own default ceiling, so any
/// file the `zstd` CLI decompresses without extra flags uploads too. The
/// decoder allocates the declared window up front; uploads are few and
/// concurrency-limited, unlike realtime bodies, which allow 8 MiB.
const ZSTD_WINDOW_LOG_MAX: u32 = 27;

/// What a file part turned out to contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Detected {
    Plain,
    Gzip,
    Zstd,
    /// A compression format we recognise but don't decode, by name.
    Unsupported(&'static str),
}

impl Detected {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Detected::Plain => "uncompressed",
            Detected::Gzip => "gzip",
            Detected::Zstd => "zstd",
            Detected::Unsupported(name) => name,
        }
    }

    /// The file name the decoded file is stored under: `batch.jsonl.gz` is
    /// stored as `batch.jsonl`, since downloads return decoded content.
    pub(crate) fn decoded_file_name(self, name: &str) -> String {
        let suffixes: &[&str] = match self {
            Detected::Gzip => &[".gz", ".gzip"],
            Detected::Zstd => &[".zst", ".zstd"],
            Detected::Plain | Detected::Unsupported(_) => &[],
        };
        suffixes
            .iter()
            .find_map(|suffix| name.strip_suffix(suffix))
            .unwrap_or(name)
            .to_string()
    }
}

/// Identifies the format from the first bytes of a file.
pub(crate) fn detect(prefix: &[u8]) -> Detected {
    // A zstd stream may open with a skippable frame (magic 0x184D2A50 to
    // 0x184D2A5F, little-endian) before its data frame; the decoder skips it.
    if let [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..] = prefix {
        return Detected::Zstd;
    }
    const SIGNATURES: &[(&[u8], Detected)] = &[
        (&[0x1f, 0x8b], Detected::Gzip),
        (&[0x28, 0xb5, 0x2f, 0xfd], Detected::Zstd),
        (&[0xfd, b'7', b'z', b'X', b'Z', 0x00], Detected::Unsupported("xz")),
        (b"BZh", Detected::Unsupported("bzip2")),
        (b"PK\x03\x04", Detected::Unsupported("zip")),
        (&[0x04, 0x22, 0x4d, 0x18], Detected::Unsupported("lz4")),
    ];
    SIGNATURES
        .iter()
        .find(|(signature, _)| prefix.starts_with(signature))
        .map_or(Detected::Plain, |(_, detected)| *detected)
}

/// A chunk of decoded file content could not be produced.
#[derive(Debug)]
pub(crate) enum ChunkError<E> {
    /// Reading the upload itself failed: passed through untouched, so callers
    /// can still recognise a body length limit.
    Upload(E),
    /// The content is not valid data for the format it starts as, including
    /// a stream that ends mid-frame.
    Decode(io::Error),
}

pub(crate) type Chunks<'a, E> = Pin<Box<dyn Stream<Item = Result<Bytes, ChunkError<E>>> + Send + 'a>>;

/// Reads just enough of `upload` to identify its format, then returns the
/// format and its content as decoded chunks. An unsupported format's chunks are
/// returned as they are, so the caller decides how to refuse it. A read error
/// while identifying is not lost: it comes out of the returned stream. The
/// caller enforces the decoded size.
pub(crate) async fn open<'a, S, E>(mut upload: S) -> (Detected, Chunks<'a, E>)
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'a,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut head = Vec::new();
    let mut prefix = Vec::with_capacity(SNIFF_LEN);
    while prefix.len() < SNIFF_LEN {
        match upload.next().await {
            Some(Ok(chunk)) => {
                prefix.extend_from_slice(&chunk[..chunk.len().min(SNIFF_LEN - prefix.len())]);
                head.push(Ok(chunk));
            }
            Some(Err(error)) => {
                head.push(Err(error));
                break;
            }
            None => break,
        }
    }
    let detected = detect(&prefix);
    let upload = stream::iter(head).chain(upload);

    let decoded: Chunks<'a, E> = match detected {
        Detected::Plain | Detected::Unsupported(_) => Box::pin(upload.map_err(ChunkError::Upload)),
        Detected::Gzip | Detected::Zstd => {
            let reader = StreamReader::new(upload.map_err(io::Error::other));
            let decoder: Pin<Box<dyn tokio::io::AsyncRead + Send + 'a>> = if detected == Detected::Gzip {
                let mut decoder = GzipDecoder::new(reader);
                decoder.multiple_members(true);
                Box::pin(decoder)
            } else {
                let mut decoder =
                    ZstdDecoder::with_params(reader, &[async_compression::zstd::DParameter::window_log_max(ZSTD_WINDOW_LOG_MAX)]);
                // Like gzip members, concatenated zstd frames are one file.
                decoder.multiple_members(true);
                Box::pin(decoder)
            };
            Box::pin(ReaderStream::with_capacity(decoder, DECODED_CHUNK).map_err(upload_or_decode_error))
        }
    };
    (detected, decoded)
}

/// Splits the decoder's I/O errors back into the upload's own errors, which
/// the reader wrapped, and errors in the compressed data itself.
fn upload_or_decode_error<E: std::error::Error + Send + Sync + 'static>(error: io::Error) -> ChunkError<E> {
    if error.get_ref().is_some_and(|inner| inner.is::<E>()) {
        let inner = error.into_inner().expect("checked above");
        return ChunkError::Upload(*inner.downcast::<E>().expect("checked above"));
    }
    ChunkError::Decode(error)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const JSONL: &[u8] = b"{\"custom_id\":\"a\"}\n{\"custom_id\":\"b\"}\n";

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// Feeds `data` in `size`-byte chunks, so the signature can straddle them.
    fn upload(data: Vec<u8>, size: usize) -> impl Stream<Item = Result<Bytes, io::Error>> + Send + Unpin {
        let chunks: Vec<_> = data.chunks(size).map(|chunk| Ok(Bytes::copy_from_slice(chunk))).collect();
        stream::iter(chunks)
    }

    async fn read_all(data: Vec<u8>, size: usize) -> (Detected, Result<Vec<u8>, ChunkError<io::Error>>) {
        let (detected, mut chunks) = open(upload(data, size)).await;
        let mut out = Vec::new();
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(error) => return (detected, Err(error)),
            }
        }
        (detected, Ok(out))
    }

    #[tokio::test]
    async fn decodes_gzip_and_zstd_files_whatever_the_chunking() {
        let mut two_members = gzip(&JSONL[..18]);
        two_members.extend(gzip(&JSONL[18..]));
        let mut two_frames = zstd::encode_all(&JSONL[..18], 3).unwrap();
        two_frames.extend(zstd::encode_all(&JSONL[18..], 3).unwrap());
        // A skippable frame (magic, 4-byte length, payload) ahead of the data.
        let mut skippable_first = vec![0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, b'x', b'y', b'z'];
        skippable_first.extend(zstd::encode_all(JSONL, 3).unwrap());
        for (compressed, expected) in [
            (gzip(JSONL), Detected::Gzip),
            (two_members, Detected::Gzip),
            (two_frames, Detected::Zstd),
            (skippable_first, Detected::Zstd),
            (zstd::encode_all(JSONL, 3).unwrap(), Detected::Zstd),
            (JSONL.to_vec(), Detected::Plain),
        ] {
            for size in [1, 3, 1024] {
                let (detected, decoded) = read_all(compressed.clone(), size).await;
                assert_eq!(detected, expected);
                assert_eq!(decoded.unwrap(), JSONL, "{expected:?} in {size}-byte chunks");
            }
        }
    }

    #[tokio::test]
    async fn reports_truncated_and_corrupt_files_as_decode_errors() {
        let gz = gzip(JSONL);
        let zst = zstd::encode_all(JSONL, 3).unwrap();
        let mut corrupt = gz.clone();
        let middle = corrupt.len() / 2;
        corrupt[middle] ^= 0xff;
        // A later frame is checked like the first, not ignored.
        let mut later_frame_truncated = zst.clone();
        later_frame_truncated.extend_from_slice(&zst[..zst.len() - 3]);
        for broken in [
            gz[..gz.len() - 4].to_vec(),
            zst[..zst.len() - 3].to_vec(),
            corrupt,
            later_frame_truncated,
        ] {
            let (_, decoded) = read_all(broken, 7).await;
            assert!(matches!(decoded, Err(ChunkError::Decode(_))), "{decoded:?}");
        }
    }

    #[tokio::test]
    async fn passes_upload_errors_through_untouched() {
        for data in [gzip(JSONL), JSONL.to_vec()] {
            let failing = upload(data[..4].to_vec(), 2).chain(stream::iter([Err(io::Error::other("reset"))]));
            let (_, mut chunks) = open(failing).await;
            let mut saw = None;
            while let Some(chunk) = chunks.next().await {
                if let Err(error) = chunk {
                    saw = Some(error);
                    break;
                }
            }
            match saw {
                Some(ChunkError::Upload(error)) => assert_eq!(error.to_string(), "reset"),
                other => panic!("expected the upload error, got {other:?}"),
            }
        }
    }

    #[test]
    fn names_formats_it_does_not_decode() {
        assert_eq!(detect(b"\xfd7zXZ\x00rest"), Detected::Unsupported("xz"));
        assert_eq!(detect(b"BZh91AY"), Detected::Unsupported("bzip2"));
        assert_eq!(detect(b"PK\x03\x04"), Detected::Unsupported("zip"));
        assert_eq!(detect(b"\x04\x22\x4d\x18"), Detected::Unsupported("lz4"));
        assert_eq!(detect(b"{\"custom_id\""), Detected::Plain);
        assert_eq!(detect(&[0x5a, 0x2a, 0x4d, 0x18, 0, 0]), Detected::Zstd);
        assert_eq!(detect(b""), Detected::Plain);
    }

    #[test]
    fn stores_decoded_files_without_the_compression_suffix() {
        assert_eq!(Detected::Gzip.decoded_file_name("batch.jsonl.gz"), "batch.jsonl");
        assert_eq!(Detected::Zstd.decoded_file_name("batch.jsonl.zst"), "batch.jsonl");
        assert_eq!(Detected::Gzip.decoded_file_name("batch.jsonl"), "batch.jsonl");
        assert_eq!(Detected::Plain.decoded_file_name("batch.jsonl.gz"), "batch.jsonl.gz");
    }
}
