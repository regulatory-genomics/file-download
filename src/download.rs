use std::{fs::File, io::{BufReader, Read, Write}, path::{Path, PathBuf}};
use std::time::{SystemTime, UNIX_EPOCH};
use flate2::read::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use platform_dirs::AppDirs;
use reqwest::Client;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use anyhow::{bail, Context, Result};

pub struct Downloader {
    cache_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decompression {
    Gzip,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extraction {
    Tar,
}

impl Downloader {
    /// Creates a new `Downloader` with the specified cache directory.
    pub fn new(cache_dir: Option<impl Into<PathBuf>>) -> Result<Self> {
        let cache_dir = if let Some(dir) = cache_dir {
            let dir = dir.into();
            if !dir.exists() {
                std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create cache directory"))?;
            }
            dir
        } else {
            let dir = AppDirs::new(Some("file-download-rs"), true).unwrap().cache_dir;
            if !dir.exists() {
                std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create cache directory"))?;
            }
            dir
        };
        Ok(Self { cache_dir })
    }

    pub fn retrieve(
        &self,
        url: &str,
        filename: Option<&str>,
        known_hash: Option<&[u8]>,
        progress_bar: bool,
        decompression: Option<Decompression>,
    ) -> Result<PathBuf> {
        let rt = tokio::runtime::Runtime::new()
            .context("Failed to create Tokio runtime")?;
        rt.block_on(self.retrieve_async(url, filename, known_hash, progress_bar, decompression))
            .context("Failed to download file asynchronously")
    }

    /// Downloads a file from `url` and saves it to `output_path`.
    pub async fn retrieve_async(
        &self,
        url: &str,
        filename: Option<&str>,
        known_hash: Option<&[u8]>,
        progress_bar: bool,
        decompression: Option<Decompression>,
    ) -> Result<PathBuf> {
        let file_path = if let Some(name) = filename {
            self.cache_location(name)
        } else {
            self.cache_location(&request_filename(url).await?)
        };
        if !check_file(&file_path, known_hash)? {
            let mut digest = Sha256::new();
            let client = Client::new();
            let response = client.get(url).send().await?;
            if !response.status().is_success() {
                bail!("Request failed with status: {}", response.status());
            }

            let total_size = response.content_length();
            let pb = match total_size {
                Some(size) => ProgressBar::new(size),
                None => ProgressBar::new_spinner(),
            };

            if progress_bar {
                pb.set_style(match total_size {
                    Some(_) => ProgressStyle::default_bar()
                        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                        .expect("Invalid template")
                        .progress_chars("#>-"),
                    None => ProgressStyle::default_spinner()
                        .template("{spinner:.blue} {bytes} {binary_bytes_per_sec} ({elapsed})")
                        .expect("Invalid template"),
                });
            }

            let mut stream = response.bytes_stream();
            let partial_path = file_path.with_extension(format!(
                "{}.part",
                file_path.extension().and_then(|x| x.to_str()).unwrap_or("download")
            ));
            let mut file = tokio::fs::File::create(&partial_path).await?;

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                digest.update(chunk.as_ref());
                file.write_all(&chunk).await?;

                if progress_bar {
                    pb.inc(chunk.len() as u64);
                }
            }
            
            file.flush().await?;
            if let Some(hash) = known_hash {
                if digest.finalize().as_slice() != hash {
                    let _ = tokio::fs::remove_file(&partial_path).await;
                    bail!("Downloaded file hash does not match expected hash");
                }
            }
            tokio::fs::rename(&partial_path, &file_path).await?;
        }

        match decompression {
            None => Ok(file_path),
            Some(kind) => self.decompress_cached(&file_path, kind),
        }
    }

    pub fn retrieve_with_extraction(
        &self,
        url: &str,
        filename: Option<&str>,
        known_hash: Option<&[u8]>,
        progress_bar: bool,
        decompression: Option<Decompression>,
        extraction: Option<Extraction>,
    ) -> Result<Vec<PathBuf>> {
        let source = self.retrieve(url, filename, known_hash, progress_bar, decompression)?;
        match extraction {
            None => Ok(vec![source]),
            Some(Extraction::Tar) => self.extract_tar_cached(&source),
        }
    }

    fn cache_location(&self, filename: &str) -> PathBuf {
        self.cache_dir.join(filename)
    }

    fn decompress_cached(&self, source: &Path, kind: Decompression) -> Result<PathBuf> {
        let output = decompressed_path(source, kind);
        if output.exists() {
            return Ok(output);
        }

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temporary = output.with_extension(format!(
            "{}.{}.part",
            output.extension().and_then(|x| x.to_str()).unwrap_or("file"),
            suffix
        ));

        let result = (|| -> Result<()> {
            let input = File::open(source)?;
            let mut reader: Box<dyn Read> = match kind {
                Decompression::Gzip => Box::new(GzDecoder::new(input)),
                Decompression::Zstd => Box::new(zstd::stream::read::Decoder::new(input)?),
            };
            let mut output_file = File::create(&temporary)?;
            std::io::copy(&mut reader, &mut output_file)?;
            output_file.flush()?;
            output_file.sync_all()?;
            std::fs::rename(&temporary, &output)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map(|_| output)
    }

    fn extract_tar_cached(&self, source: &Path) -> Result<Vec<PathBuf>> {
        let directory = source.with_extension("tar.extract");
        if directory.exists() {
            return collect_files(&directory);
        }
        let temporary = source.with_extension("tar.extract.part");
        if temporary.exists() {
            std::fs::remove_dir_all(&temporary)?;
        }
        std::fs::create_dir_all(&temporary)?;
        let result = (|| -> Result<()> {
            let input = File::open(source)?;
            let mut archive = tar::Archive::new(input);
            archive.unpack(&temporary)?;
            std::fs::rename(&temporary, &directory)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temporary);
        }
        result.and_then(|_| collect_files(&directory))
    }
}

fn collect_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(collect_files(&path)?);
        } else {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn decompressed_path(source: &Path, kind: Decompression) -> PathBuf {
    let expected_suffix = match kind {
        Decompression::Gzip => "gz",
        Decompression::Zstd => "zst",
    };
    if source.extension().and_then(|x| x.to_str()) == Some(expected_suffix)
        || (kind == Decompression::Zstd
            && source.extension().and_then(|x| x.to_str()) == Some("zstd"))
    {
        source.with_extension("")
    } else {
        source.with_file_name(format!(
            "{}.decompressed",
            source.file_name().unwrap().to_string_lossy()
        ))
    }
}

fn check_file(
    file_path: impl AsRef<Path>,
    known_hash: Option<&[u8]>,
) -> Result<bool> {
    if let Some(hash) = known_hash {
        check_digest(file_path, hash)
    } else {
        if file_path.as_ref().exists() {
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

async fn request_filename(
    url: &str,
) -> Result<String> {
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        bail!("Request failed with status: {}", response.status());
    }

    Ok(response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|cd| cd.to_str().ok())
        .and_then(parse_filename_from_content_disposition)
        .unwrap_or_else(|| {
            // Fallback: use last segment of the URL path
            url.split('/')
                .last()
                .filter(|s| !s.is_empty())
                .unwrap_or("downloaded_file")
                .to_string()
        }))
}

/// Helper to parse Content-Disposition to extract filename.
fn parse_filename_from_content_disposition(header_value: &str) -> Option<String> {
    for part in header_value.split(';') {
        let part = part.trim();
        if let Some(filename_part) = part.strip_prefix("filename=") {
            // Remove surrounding quotes if present
            return Some(filename_part.trim_matches('"').to_string());
        }
    }
    None
}

fn check_digest(
    filename: impl AsRef<Path>,
    expected_digest: &[u8],
) -> Result<bool> {
    if !filename.as_ref().exists() {
        Ok(false)
    } else {
        let file = File::open(filename)?;
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buffer = [0; 4096];

        while let Ok(bytes_read) = reader.read(&mut buffer) {
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        Ok(hasher.finalize().as_slice() == expected_digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;
    use tempfile::tempdir;

    #[test]
    fn test_download_file_async() {
        let url = "https://osf.io/download/6uet5/";
        let hash = hex!("400dd60ca61dc8388aa0942b42c95920aad7f6bedf5324005cee7e84bcf5b6d0");
        let temp_dir = tempdir().unwrap();
        let downloader = Downloader::new(Some(temp_dir.path())).unwrap();
        downloader.retrieve(url, None, Some(&hash), false, None).unwrap();
    }
}
