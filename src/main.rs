use clap::{Arg, Command};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Semaphore};
use url::Url;
#[derive(Serialize, Deserialize, Debug)]
struct DownloadState {
    url: String,
    total_size: u64,
    downloaded_chunks: Vec<bool>,
    chunk_size: u64,
}

const DEFAULT_CHUNK_SIZE: u64 = 1024 * 1024;
const MIN_CHUNK_SIZE: u64 = 16 * 1024;
const DEFAULT_TIMEOUT: u64 = 300;
const MAX_RETRIES: u64 = 3;

struct DownloadManager {
    client: Client,
    num_connections: usize,
    chunk_size: u64,
    download_dir: PathBuf,
    keep_partial: bool,
}

impl DownloadManager {
    fn new(num_connections: usize, chunk_size: u64, download_dir: PathBuf, keep_partial: bool) -> Self {
        DownloadManager {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT))
                .build()
                .unwrap(),
            num_connections,
            chunk_size: chunk_size.max(MIN_CHUNK_SIZE),
            download_dir,
            keep_partial,
        }
    }

    async fn download(&self, url: &str) -> Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse(url)?;
        let filename = url
            .path_segments()
            .and_then(|segments| segments.last())
            .unwrap_or("download")
            .to_string();

        let file_path = self.download_dir.join(&filename);
        let state_path = file_path.with_extension("dlstate");

        // Create download directory if it doesn't exist
        fs::create_dir_all(&self.download_dir)?;

        // Check if the server supports resumable downloads
        let resp = self.client.head(url.as_str()).send().await?;
        let supports_resume = resp
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .map(|value| value == "bytes")
            .unwrap_or(false);

        if !supports_resume {
            eprintln!("Warning: The server does not support resumable downloads. Falling back to single connection.");
            return self.download_with_single_connection(url, &file_path).await;
        }

        // Check if we can resume a previous download
        let download_state = if state_path.exists() && self.keep_partial {
            let state_content = fs::read_to_string(&state_path)?;
            let state: DownloadState = serde_json::from_str(&state_content)?;

            // Verify URL matches
            if state.url != url.to_string() {
                fs::remove_file(&state_path)?;
                if file_path.exists() {
                    fs::remove_file(&file_path)?;
                }
                self.create_new_download_state(&url).await?
            } else {
                state
            }
        } else {
            if state_path.exists() {
                fs::remove_file(&state_path)?;
            }
            if file_path.exists() {
                fs::remove_file(&file_path)?;
            }
            self.create_new_download_state(&url).await?
        };

        // Create or open the output file
        let file = Arc::new(Mutex::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&file_path)?,
        ));

        // Set up progress display
        let multi_progress = MultiProgress::new();
        let total_progress = multi_progress.add(ProgressBar::new(download_state.total_size));
        total_progress.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("#>-"),
        );

        // Set initial progress
        let initial_progress: u64 = download_state.downloaded_chunks
            .iter()
            .enumerate()
            .filter(|(_, &downloaded)| downloaded)
            .map(|(i, _)| {
                let start = i as u64 * download_state.chunk_size;
                let end = std::cmp::min(
                    start + download_state.chunk_size,
                    download_state.total_size,
                );
                end - start
            })
            .sum();
        total_progress.inc(initial_progress);
        
        let chunks: Vec<(usize, u64, u64)> = download_state.downloaded_chunks
            .iter()
            .enumerate()
            .filter(|(_, &downloaded)| !downloaded)
            .map(|(i, _)| {
                let start = i as u64 * download_state.chunk_size;
                let end = std::cmp::min(
                    start + download_state.chunk_size,
                    download_state.total_size,
                );
                (i, start, end)
            })
            .collect();

        if chunks.is_empty() {
            println!("Download already completed!");
            return Ok(());
        }
        
        let semaphore = Arc::new(Semaphore::new(self.num_connections));
        let state = Arc::new(Mutex::new(download_state));
        let mut handles = vec![];
        
        let (shutdown_tx, _) = broadcast::channel(1);
        
        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                let _ = shutdown_tx_clone.send(());
            }
        });

        // Download chunks
        for (chunk_index, start, end) in chunks {
            let url = url.clone();
            let client = self.client.clone();
            let file = file.clone();
            let semaphore = semaphore.clone();
            let total_progress = total_progress.clone();
            let state = state.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let state_path = state_path.clone();

            let handle = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                let mut retries = MAX_RETRIES;
                let mut success = false;

                while retries > 0 && !success {
                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(
                        reqwest::header::RANGE,
                        format!("bytes={}-{}", start, end - 1).parse().unwrap(),
                    );

                    let response = client
                        .get(url.as_str())
                        .headers(headers)
                        .send()
                        .await;

                    match response {
                        Ok(mut resp) => {
                            let mut buf = Vec::with_capacity((end - start) as usize);
                            loop {
                                tokio::select! {
                                    chunk = resp.chunk() => {
                                        match chunk {
                                            Ok(Some(data)) => {
                                                buf.extend_from_slice(&data);
                                                total_progress.inc(data.len() as u64);
                                            }
                                            Ok(None) => {
                                                success = true;
                                                break;
                                            }
                                            Err(e) => {
                                                eprintln!("Chunk download failed: {}", e);
                                                break;
                                            }
                                        }
                                    }
                                    _ = shutdown_rx.recv() => {
                                        return Err("Download interrupted by user".into());
                                    }
                                }
                            }

                            if success {
                                let mut file = file.lock().await;
                                let mut writer = BufWriter::new(&mut *file);
                                writer.seek(SeekFrom::Start(start))?;
                                writer.write_all(&buf)?;
                                writer.flush()?;

                                // Update download state
                                let mut state = state.lock().await;
                                state.downloaded_chunks[chunk_index] = true;

                                // Save state to file
                                let state_json = serde_json::to_string(&*state)?;
                                fs::write(&state_path, state_json)?;
                            }
                        }
                        Err(e) => {
                            eprintln!("Chunk request failed: {}", e);
                        }
                    }

                    retries -= 1;
                }

                if !success {
                    return Err("Failed to download chunk after retries".into());
                }

                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });

            handles.push(handle);
        }
        
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => {
                    if !self.keep_partial {
                        // Clean up partial download
                        let _ = fs::remove_file(&file_path);
                        let _ = fs::remove_file(&state_path);
                    }
                    return Err(e);
                }
                Err(e) => {
                    if !self.keep_partial {
                        // Clean up partial download
                        let _ = fs::remove_file(&file_path);
                        let _ = fs::remove_file(&state_path);
                    }
                    return Err(e.into());
                }
            }
        }

        // Clean up state file after successful download
        let _ = fs::remove_file(&state_path);

        // Finish progress bar
        total_progress.finish_with_message("Download complete");

        Ok(())
    }

    async fn create_new_download_state(&self, url: &Url) -> Result<DownloadState, Box<dyn std::error::Error>> {
        let resp = self.client.head(url.as_str()).send().await?;

        let total_size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|ct_len| ct_len.to_str().ok())
            .and_then(|ct_len| ct_len.parse().ok())
            .ok_or("Could not determine file size")?;

        let chunk_size = self.chunk_size.min(total_size);
        let num_chunks = (total_size + chunk_size - 1) / chunk_size;

        Ok(DownloadState {
            url: url.to_string(),
            total_size,
            downloaded_chunks: vec![false; num_chunks as usize],
            chunk_size,
        })
    }

    async fn download_with_single_connection(&self, url: Url, file_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        let mut response = self.client.get(url.as_str()).send().await?;

        // Get the total file size from the response
        let total_size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|ct_len| ct_len.to_str().ok())
            .and_then(|ct_len| ct_len.parse::<u64>().ok())
            .unwrap_or(0);

        // Create a progress bar
        let progress_bar = ProgressBar::new(total_size);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("#>-"),
        );

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(file_path)?;

        let mut downloaded: u64 = 0;
       // let mut buf: Vec<u8> = Vec::new();

        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk)?;
            downloaded += chunk.len() as u64;
            progress_bar.set_position(downloaded);
        }

        progress_bar.finish_with_message("Download complete");

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("dlm")
        .version("1.0")
        .author("Download Manager")
        .about("Downloads files with multiple connections and resume support")
        .arg(
            Arg::new("connections")
                .short('x')
                .long("connections")
                .value_name("NUM")
                .help("Number of concurrent connections")
                .default_value("4"),
        )
        .arg(
            Arg::new("chunk-size")
                .short('s')
                .long("chunk-size")
                .value_name("SIZE")
                .help("Chunk size in bytes (minimum 16KB)")
                .default_value("1048576"),
        )
        .arg(
            Arg::new("keep-partial")
                .short('k')
                .long("keep-partial")
                .help("Keep partial downloads on failure")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dir")
                .short('d')
                .long("dir")
                .value_name("DIR")
                .help("Download directory")
                .default_value("."),
        )
        .arg(
            Arg::new("url")
                .help("URL to download")
                .required(true)
                .index(1),
        )
        .get_matches();

    let num_connections = matches
        .get_one::<String>("connections")
        .unwrap()
        .parse()
        .unwrap_or(4);
    let chunk_size = matches
        .get_one::<String>("chunk-size")
        .unwrap()
        .parse()
        .unwrap_or(DEFAULT_CHUNK_SIZE);
    let keep_partial = matches.get_flag("keep-partial");
    let download_dir = PathBuf::from(matches.get_one::<String>("dir").unwrap());
    let url = matches.get_one::<String>("url").unwrap();

    let downloader = DownloadManager::new(
        num_connections,
        chunk_size,
        download_dir,
        keep_partial,
    );

    if let Err(e) = downloader.download(url).await {
        eprintln!("Download failed: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
