use super::*;
use anyhow::Result;
use ring::digest::{SHA256, digest};
use serde::Serialize;
use std::io::Read;
use std::num::NonZeroU32;
use std::sync::Mutex as StdMutex;
use zstd::bulk::{Compressor, Decompressor};

pub(super) const LEVEL: i32 = 3;
pub(super) const MAX_DICTIONARY: usize = 64 * 1024;

pub(super) fn compressor(dictionary: &[u8]) -> std::io::Result<Compressor<'static>> {
    let mut compressor = Compressor::with_dictionary(LEVEL, dictionary)?;
    compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
    Ok(compressor)
}

pub(super) fn directory() -> Result<PathBuf> {
    Ok(Config::profile_candidate(None)
        .context("Cannot locate the Ratio user configuration directory")?
        .with_file_name("web-dictionaries"))
}

#[derive(Clone, Serialize)]
pub(super) struct Descriptor {
    pub sha256: String,
    pub id: NonZeroU32,
    pub bytes: u32,
}

pub(super) struct Dictionary {
    pub descriptor: Descriptor,
    pub bootstrap: Bytes,
    pub encoder: Encoder,
}

#[derive(Default)]
pub(super) struct Dictionaries {
    pub operator: Option<Arc<Dictionary>>,
    pub shared: Option<Arc<Dictionary>>,
}

impl Dictionaries {
    pub fn load() -> Self {
        let Ok(dir) = directory() else {
            return Self::default();
        };
        let load = |name| match Dictionary::load(&dir.join(name)) {
            Ok(dictionary) => Some(Arc::new(dictionary)),
            Err(error) => {
                if !error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    eprintln!("Ignoring dashboard dictionary {name}: {error:#}");
                }
                None
            }
        };
        Self {
            operator: load("operator.zdict"),
            shared: load("shared.zdict"),
        }
    }
}

impl Dictionary {
    fn load(path: &std::path::Path) -> Result<Self> {
        ensure!(
            std::fs::symlink_metadata(path)?.is_file(),
            "Dictionary must be a regular file"
        );
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Dictionary must be a regular file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
                "Dictionary must be private to the current user"
            );
        }
        let mut bytes = Vec::new();
        file.take((MAX_DICTIONARY + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_DICTIONARY,
            "Dictionary exceeds its size limit"
        );
        let id = zstd::zstd_safe::get_dict_id_from_dict(&bytes)
            .context("Invalid Zstandard dictionary")?;
        let sha256 = digest(&SHA256, &bytes)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let encoder = Encoder::new(&bytes)?;
        let example = b"Ratio dictionary verification";
        let encoded = encoder.compress(|compressor| compressor.compress(example))?;
        ensure!(
            Decompressor::with_dictionary(&bytes)?.decompress(&encoded, example.len())? == example,
            "Dictionary did not round-trip"
        );
        Ok(Self {
            descriptor: Descriptor {
                sha256,
                id,
                bytes: bytes.len() as u32,
            },
            bootstrap: super::realtime::zstd_frame(
                6,
                0,
                bytes.len(),
                &compressor(&[])?.compress(&bytes)?,
            ),
            encoder,
        })
    }
}

pub(super) struct Encoder([StdMutex<Compressor<'static>>; 2]);

impl Encoder {
    pub fn new(dictionary: &[u8]) -> Result<Self> {
        Ok(Self([
            StdMutex::new(compressor(dictionary)?),
            StdMutex::new(compressor(dictionary)?),
        ]))
    }

    pub fn compress<T>(
        &self,
        use_encoder: impl FnOnce(&mut Compressor<'static>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut encoder = self.0[0]
            .try_lock()
            .unwrap_or_else(|_| self.0[1].lock().expect("dashboard encoder"));
        use_encoder(&mut encoder)
    }
}
