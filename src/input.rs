//! Per-file input handle. Plain files always go through `Plain`; with the
//! `zeekstd` feature, `.zst`/`.zstd`/`.seekztd` paths first try
//! `zeekstd::Decoder` (full bisect/parallel/follow) and on missing seek-table
//! footer fall back to a forward-only `zstd::stream::read::Decoder` that
//! reuses the stdin streaming path. With the feature disabled, every path
//! opens as a plain file.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[cfg(feature = "zeekstd")]
use std::io::BufReader;

#[cfg(feature = "zeekstd")]
pub use zeekstd::SeekTable;

/// Stub when the `zeekstd` feature is off so call sites can carry an
/// `Option<SeekTable>` field unconditionally. Never constructed.
#[cfg(not(feature = "zeekstd"))]
pub enum SeekTable {}

pub enum FileInput {
    Plain(File),
    #[cfg(feature = "zeekstd")]
    Seekable(Box<zeekstd::Decoder<'static, File>>),
    #[cfg(feature = "zeekstd")]
    Streaming(Box<zstd::stream::read::Decoder<'static, BufReader<File>>>),
}

#[cfg(feature = "zeekstd")]
fn has_zstd_ext(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("zst") | Some("zstd") | Some("seekztd")
    )
}

impl FileInput {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_table(path, None)
    }

    /// Open with a preparsed seek table (workers reuse the master's table to
    /// skip a footer round-trip per thread). `table` is ignored for non-zstd
    /// paths; always `None` when the `zeekstd` feature is off.
    #[cfg_attr(not(feature = "zeekstd"), allow(unused_variables))]
    pub fn open_with_table(path: &Path, table: Option<&SeekTable>) -> anyhow::Result<Self> {
        #[cfg(not(feature = "zeekstd"))]
        {
            assert!(table.is_none());
            Ok(Self::Plain(File::open(path)?))
        }
        #[cfg(feature = "zeekstd")]
        {
            if !has_zstd_ext(path) {
                return Ok(Self::Plain(File::open(path)?));
            }
            let file = File::open(path)?;
            if let Some(t) = table {
                let dec = zeekstd::DecodeOptions::new(file)
                    .seek_table(t.clone())
                    .into_decoder()?;
                return Ok(Self::Seekable(Box::new(dec)));
            }
            let mut probe = File::open(path)?;
            match zeekstd::Decoder::new(probe.try_clone()?) {
                Ok(dec) => Ok(Self::Seekable(Box::new(dec))),
                Err(e) => {
                    log::warn!(
                        "{}: zstd input has no seek table ({e}); falling back to streaming \
                         decode (no bisect, no parallel, no follow). Re-compress with \
                         `zeekstd compress` to enable the fast path.",
                        path.display()
                    );
                    probe.seek(SeekFrom::Start(0))?;
                    let dec = zstd::stream::read::Decoder::new(probe)?;
                    Ok(Self::Streaming(Box::new(dec)))
                }
            }
        }
    }

    pub fn supports_seek(&self) -> bool {
        #[cfg(feature = "zeekstd")]
        {
            !matches!(self, Self::Streaming(_))
        }
        #[cfg(not(feature = "zeekstd"))]
        {
            true
        }
    }

    /// Clone the parsed seek table out of an already-open seekable input.
    /// Returns `None` for plain files, streaming-zstd, and (trivially) when
    /// the feature is off. Callers stash this on the `FilePlan` so `-j`
    /// workers skip the footer round-trip on reopen.
    pub fn seek_table(&self) -> Option<SeekTable> {
        #[cfg(feature = "zeekstd")]
        match self {
            Self::Seekable(d) => Some(d.seek_table().clone()),
            _ => None,
        }
        #[cfg(not(feature = "zeekstd"))]
        match *self {
            Self::Plain(_) => None,
        }
    }
}

impl Read for FileInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(f) => f.read(buf),
            #[cfg(feature = "zeekstd")]
            Self::Seekable(d) => d.read(buf),
            #[cfg(feature = "zeekstd")]
            Self::Streaming(d) => d.read(buf),
        }
    }
}

impl Seek for FileInput {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Plain(f) => f.seek(pos),
            #[cfg(feature = "zeekstd")]
            Self::Seekable(d) => d.seek(pos),
            #[cfg(feature = "zeekstd")]
            Self::Streaming(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "streaming zstd input is not seekable",
            )),
        }
    }
}
