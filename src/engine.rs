use crate::{
    Cancel, File, Listing, Site, sites,
    state::{Partial, State, source_key},
    transport::{RetryOptions, SitePolicy, Transient, Transport, network_error},
};
use anyhow::{Result, bail};
use futures_util::{StreamExt, stream};
use md5::Md5;
use reqwest::header;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    List,
    Download,
    Refresh,
    Update,
}
pub struct RunOptions {
    pub site: Site,
    pub urls: Vec<String>,
    pub root: PathBuf,
    pub action: Action,
    pub workers: usize,
    pub password: Option<String>,
}
#[derive(Clone, Debug)]
pub enum FileOutcome {
    Downloaded,
    Skipped,
    Failed(String),
    Cancelled,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub downloaded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub pending: usize,
}
#[derive(Clone, Debug)]
pub enum Event {
    SourceChanged {
        previous_path: String,
        file: File,
    },
    Message(String),
    Listing {
        url: String,
        listing: Listing,
    },
    Progress {
        path: String,
        received: u64,
        total: Option<u64>,
    },
    FileDone {
        path: String,
        outcome: FileOutcome,
    },
    Finished(Summary),
}
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;
pub struct Engine {
    pub options: RunOptions,
    pub state: Arc<State>,
    pub cancel: Cancel,
    pub sink: Sink,
    pub transport: Transport,
    recu_signin_required: AtomicBool,
    recu_browser_unavailable: AtomicBool,
    recu_browser: Arc<Mutex<Option<crate::browser::BrowserSession>>>,
    recu_transfer: Arc<tokio::sync::Semaphore>,
    recu_conversion: Arc<tokio::sync::Semaphore>,
}
impl Engine {
    pub fn new(options: RunOptions, cancel: Cancel, sink: Sink) -> Result<Arc<Self>> {
        let callback = sink.clone();
        let transport = Transport::new(
            Arc::new(SitePolicy(options.site)),
            cancel.clone(),
            Arc::new(move |s| callback(Event::Message(s))),
            RetryOptions::default(),
        )?;
        Self::with_transport(options, cancel, sink, transport)
    }
    pub fn with_transport(
        mut options: RunOptions,
        cancel: Cancel,
        sink: Sink,
        transport: Transport,
    ) -> Result<Arc<Self>> {
        if !(1..=16).contains(&options.workers) {
            bail!("Elige entre 1 y 16 transferencias")
        }
        if options.action == Action::Update {
            bail!("Buscar nuevos no disponible para Recu.me")
        }
        options.urls = sites::inputs(options.site, &options.urls.join("\n"))?;
        let state = Arc::new(State::open(&options.root, options.site)?);
        state.import_legacy(options.site, &cancel)?;
        Ok(Arc::new(Self {
            options,
            state,
            cancel,
            sink,
            transport,
            recu_signin_required: AtomicBool::new(false),
            recu_browser_unavailable: AtomicBool::new(false),
            recu_browser: Arc::new(Mutex::new(None)),
            recu_transfer: Arc::new(tokio::sync::Semaphore::new(1)),
            recu_conversion: Arc::new(tokio::sync::Semaphore::new(1)),
        }))
    }
    fn publish(
        &self,
        file: &File,
        partial: &std::path::Path,
        size: u64,
        digest: &str,
    ) -> Result<FileOutcome> {
        self.cancel.check()?;
        let target = self.state.target(&file.path)?;
        if !file.sha256.is_empty() && !file.sha256.eq_ignore_ascii_case(digest) {
            fs::write(partial, [])?;
            self.state.clear_partial(&file.path)?;
            return Err(Transient {
                message: "SHA-256 diferente; se reinicia el parcial".into(),
                after: None,
            }
            .into());
        }
        self.state
            .record(file, size, digest, crate::mtime(partial)?, "moving")?;
        if target.exists() {
            let folder = self.state.directory.join("replaced");
            fs::create_dir_all(&folder)?;
            let name = format!(
                "{}-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                Sha256::digest(file.path.as_bytes())
            );
            let relative = folder
                .join(name)
                .strip_prefix(&self.state.root)?
                .to_string_lossy()
                .replace('\\', "/");
            let backup = crate::target(&self.state.root, &relative)?;
            fs::rename(&target, &backup)?;
            (self.sink)(Event::Message(format!(
                "Archivo anterior conservado: {}",
                backup.display()
            )));
        }
        fs::rename(partial, &target)?;
        self.state
            .record(file, size, digest, crate::mtime(&target)?, "complete")?;
        self.state.clear_partial(&file.path)?;
        Ok(FileOutcome::Downloaded)
    }

    async fn is_completed(&self, file: &File) -> Result<bool> {
        self.cancel.check()?;
        self.transport
            .policy
            .check(&Url::parse(&file.url)?, &file.post)?;
        let state = self.state.clone();
        let file = file.clone();
        let cancel = self.cancel.clone();
        tokio::task::spawn_blocking(move || state.completed(&file, &cancel)).await?
    }

    async fn finish_recu_recording(
        &self,
        file: &File,
        input: PathBuf,
        remove_input: bool,
    ) -> Result<FileOutcome> {
        let ffmpeg = recu_ffmpeg_path()?;
        let output = self
            .state
            .partial_path(&file.path)?
            .with_extension("av1.mp4");
        let cancel = self.cancel.clone();
        let sink = self.sink.clone();
        let source = input.clone();
        let destination = output.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _ = fs::remove_file(&destination);
            sink(Event::Message("Convirtiendo grabacion a MP4 AV1; esto puede tardar varios minutos.".into()));
            let threads = recu_encoder_parallelism().to_string();
            let mut command = std::process::Command::new(&ffmpeg);
            command
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-threads"])
                .arg(&threads)
                .arg("-i")
                .arg(&source)
                .args(["-map", "0:v:0", "-map", "0:a?", "-c:v", "libsvtav1", "-preset", "6", "-crf", "34", "-pix_fmt", "yuv420p", "-svtav1-params"])
                .arg(format!("lp={threads}"))
                .args(["-threads"])
                .arg(&threads)
                .args(["-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart"])
                .arg(&destination)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000 | 0x00004000);
            }
            let mut child = command.spawn()?;
            #[cfg(windows)]
            if let Err(error) = limit_recu_encoder_process(&child) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("No se pudo limitar la CPU de FFmpeg: {error}");
            }
            let status = loop {
                if cancel.is_cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = fs::remove_file(&destination);
                    cancel.check()?;
                }
                if let Some(status) = child.try_wait()? { break status }
                std::thread::sleep(std::time::Duration::from_millis(250));
            };
            if !status.success() {
                let _ = fs::remove_file(&destination);
                bail!("FFmpeg no pudo convertir esta grabacion a AV1; se conserva el archivo temporal para reintentar")
            }
            if let Err(error) = verify_recu_mp4(&ffmpeg, &destination) {
                let _ = fs::remove_file(&destination);
                return Err(error);
            }
            Ok(())
        }).await??;
        let size = output.metadata()?.len();
        if size == 0 {
            bail!("La conversion AV1 produjo un archivo vacio")
        }
        let (digest, _) = prefix_hashes(&output, &self.cancel, false)?;
        let target = self.state.target(&file.path)?;
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Destino sin carpeta"))?,
        )?;
        let outcome = self.publish(file, &output, size, &format!("{:x}", digest.finalize()))?;
        if remove_input && let Err(error) = fs::remove_file(input) {
            (self.sink)(Event::Message(format!(
                "No se pudo limpiar el temporal MPEG-TS: {error}"
            )));
        }
        Ok(outcome)
    }
    async fn recu_recording_is_completed(&self, file: &File) -> Result<bool> {
        if !self.is_completed(file).await? {
            return Ok(false);
        }
        let ffmpeg = recu_ffmpeg_path()?;
        let path = self.state.target(&file.path)?;
        match tokio::task::spawn_blocking(move || verify_recu_mp4(&ffmpeg, &path)).await? {
            Ok(()) => Ok(true),
            Err(error) => {
                (self.sink)(Event::Message(format!(
                    "MP4 anterior incompleto ({}): {error}; se intentara descargar de nuevo.",
                    file.id
                )));
                Ok(false)
            }
        }
    }
    async fn download_recu(&self, mut file: File) -> (File, Result<FileOutcome>) {
        if self.recu_browser_unavailable.load(Ordering::SeqCst) {
            return (file, Err(crate::browser::BrowserDisconnected.into()));
        }
        if self.recu_signin_required.load(Ordering::SeqCst) {
            return (file, Err(sites::recu::SignInRequired.into()));
        }
        let transfer_permit = match self.recu_transfer.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => return (file, Err(error.into())),
        };
        if let Err(error) = self.cancel.check() {
            return (file, Err(error));
        }
        if self.recu_browser_unavailable.load(Ordering::SeqCst) {
            return (file, Err(crate::browser::BrowserDisconnected.into()));
        }
        if file.id.starts_with("grabaciones:") {
            let original = file.clone();
            file.path = file
                .path
                .trim_end_matches(".ts")
                .trim_end_matches(".mp4")
                .to_owned()
                + ".mp4";
            if original.path != file.path {
                (self.sink)(Event::SourceChanged {
                    previous_path: original.path.clone(),
                    file: file.clone(),
                });
            }
            match self.recu_recording_is_completed(&file).await {
                Ok(true) => return (file, Ok(FileOutcome::Skipped)),
                Ok(false) => {}
                Err(error) => return (file, Err(error)),
            }
            let mut legacy = original;
            legacy.path = legacy
                .path
                .trim_end_matches(".ts")
                .trim_end_matches(".mp4")
                .to_owned()
                + ".ts";
            match self.is_completed(&legacy).await {
                Ok(true) => {
                    let old = match self.state.target(&legacy.path) {
                        Ok(path) => path,
                        Err(error) => return (file, Err(error)),
                    };
                    drop(transfer_permit);
                    let result = match self.recu_conversion.clone().acquire_owned().await {
                        Ok(_permit) => self.finish_recu_recording(&file, old, false).await,
                        Err(error) => Err(error.into()),
                    };
                    return (file, result);
                }
                Ok(false) => {}
                Err(error) => return (file, Err(error)),
            }
        }
        if file.id.starts_with("clips:") && file.path.ends_with(".ts") {
            let mut mp4 = file.clone();
            mp4.path = mp4.path.trim_end_matches(".ts").to_owned() + ".mp4";
            match self.is_completed(&mp4).await {
                Ok(true) => return (mp4, Ok(FileOutcome::Skipped)),
                Ok(false) => {}
                Err(error) => return (file, Err(error)),
            }
        }
        if !file.id.starts_with("grabaciones:") {
            match self.is_completed(&file).await {
                Ok(true) => return (file, Ok(FileOutcome::Skipped)),
                Ok(false) => {}
                Err(error) => return (file, Err(error)),
            }
        }
        if file.id.starts_with("grabaciones:")
            && let Err(error) = recu_ffmpeg_path()
        {
            return (file, Err(error));
        }
        (self.sink)(Event::Message("Abre Chrome e inicia sesión en Recu.me si se solicita; se usará solo la reproducción disponible para esta cuenta.".into()));
        for attempt in 0..2 {
            let state = self.state.clone();
            let cancel = self.cancel.clone();
            let sink = self.sink.clone();
            let original = file.clone();
            let previous_path = file.path.clone();
            let shared_browser = self.recu_browser.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let log: crate::Log = Arc::new(move |message| sink(Event::Message(message)));
                let mut slot = shared_browser
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Sesion de Chrome no disponible"))?;
                recover_recu_browser(
                    &mut slot,
                    || {
                        crate::browser::BrowserSession::launch_dedicated(
                            &state.directory.join("browser-profile"),
                            false,
                            cancel.clone(),
                            log.clone(),
                        )
                    },
                    |browser| sites::recu::browser_source(browser, &original, &cancel),
                    || {
                        log(
                            "Chrome se cerró; reabriendo el perfil guardado y renovando la fuente."
                                .into(),
                        )
                    },
                )
            })
            .await;
            let (source, agent) = match result {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    if error
                        .downcast_ref::<sites::recu::SignInRequired>()
                        .is_some()
                    {
                        self.recu_signin_required.store(true, Ordering::SeqCst);
                    }
                    return (file, Err(error));
                }
                Err(error) => return (file, Err(error.into())),
            };
            let extension = recu_output_extension(&file.id, &source);
            let stem = file.path.trim_end_matches(".ts").trim_end_matches(".mp4");
            file.path = format!("{stem}.{extension}");
            file.url = source;
            (self.sink)(Event::SourceChanged {
                previous_path,
                file: file.clone(),
            });
            let outcome = self
                .transport
                .retry_limited(4, || self.transfer_recu(&file, &agent))
                .await;
            let expired = outcome
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<crate::transport::HttpFailure>())
                .is_some_and(|failure| matches!(failure.0.as_u16(), 401 | 403 | 404 | 422));
            if !expired || attempt == 1 || self.cancel.is_cancelled() {
                drop(transfer_permit);
                if file.id.starts_with("grabaciones:") && outcome.is_ok() {
                    let partial = match self.state.partial_path(&file.path) {
                        Ok(path) => path,
                        Err(error) => return (file, Err(error)),
                    };
                    let result = match self.recu_conversion.clone().acquire_owned().await {
                        Ok(_permit) => self.finish_recu_recording(&file, partial, true).await,
                        Err(error) => Err(error.into()),
                    };
                    return (file, result);
                }
                return (file, outcome);
            }
            (self.sink)(Event::Message(
                "La firma multimedia caducó; se solicita una fuente nueva.".into(),
            ));
        }
        unreachable!()
    }
    async fn transfer_recu(&self, file: &File, agent: &str) -> Result<FileOutcome> {
        self.cancel.check()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(agent)
            .timeout(std::time::Duration::from_secs(90))
            .build()?;
        let is_clip = file.id.starts_with("clips:");
        let clip_id = file.id.strip_prefix("clips:").unwrap_or("");
        let validate = |url: &str| -> Result<()> {
            if is_clip {
                if url
                    .split('?')
                    .next()
                    .is_some_and(|path| path.ends_with(".mp4"))
                {
                    sites::recu::validate_clip_source(url, &file.post, clip_id)?;
                } else {
                    sites::recu::validate_clip_hls(url, &file.post, clip_id)?;
                }
            } else {
                sites::recu::validate_hls(url, &file.post)?;
            }
            Ok(())
        };
        let fetch = |url: &str| {
            client
                .get(url)
                .header(header::ORIGIN, "https://es.recu.me")
                .header(header::REFERER, &file.post)
        };
        validate(&file.url)?;
        let url = Url::parse(&file.url)?;
        let ext = url.path().rsplit('.').next().unwrap_or("");
        let target = self.state.target(&file.path)?;
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Destino sin carpeta"))?,
        )?;
        let partial = self.state.partial_path(&file.path)?;
        if ext == "mp4" {
            let mut response = fetch(&file.url)
                .send()
                .await
                .map_err(network_error)
                .and_then(recu_status)?;
            if !response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|mime| {
                    mime.to_ascii_lowercase().starts_with("video/mp4")
                        || mime
                            .to_ascii_lowercase()
                            .starts_with("application/octet-stream")
                })
            {
                bail!("El clip no devolvió MP4")
            }
            let mut output = fs::File::create(&partial)?;
            let mut digest = Sha256::new();
            let mut size = 0;
            while let Some(chunk) = response.chunk().await.map_err(network_error)? {
                self.cancel.check()?;
                output.write_all(&chunk)?;
                digest.update(&chunk);
                size += chunk.len() as u64;
                (self.sink)(Event::Progress {
                    path: file.path.clone(),
                    received: size,
                    total: response.content_length(),
                });
            }
            output.sync_all()?;
            if size == 0 {
                bail!("Clip MP4 vacío")
            }
            return self.publish(file, &partial, size, &format!("{:x}", digest.finalize()));
        }
        if ext != "m3u8" {
            bail!("Formato de Recu.me no compatible")
        }
        let mut variant = file.url.clone();
        let response = fetch(&variant)
            .send()
            .await
            .map_err(network_error)
            .and_then(recu_status)?;
        let initial = response.text().await.map_err(network_error)?;
        let body = if initial.contains("#EXT-X-STREAM-INF:") {
            variant = if is_clip {
                sites::recu::parse_clip_master(&initial, &variant, &file.post, clip_id)?
            } else {
                sites::recu::parse_master(&initial, &variant, &file.post)?
            };
            validate(&variant)?;
            let response = fetch(&variant)
                .send()
                .await
                .map_err(network_error)
                .and_then(recu_status)?;
            response.text().await.map_err(network_error)?
        } else {
            initial
        };
        let segments = if is_clip {
            sites::recu::parse_clip_segments(&body, &variant, &file.post, clip_id)?
        } else {
            sites::recu::parse_segments(&body, &variant, &file.post)?
        };
        let mut names = Sha256::new();
        names.update(Url::parse(&variant)?.path().as_bytes());
        names.update(b"\n");
        for segment in &segments {
            names.update(Url::parse(segment)?.path().as_bytes());
            names.update(b"\n");
        }
        let fingerprint = format!("{:x}", names.finalize());
        let saved = self.state.partial(&file.path)?;
        let mut completed = 0usize;
        let mut size = 0u64;
        if let Some(saved) = saved {
            let expected_prefix = format!("hls:{fingerprint}:");
            if saved.source == file.id && saved.validator.starts_with(&expected_prefix) {
                completed = saved.validator[expected_prefix.len()..]
                    .parse::<usize>()
                    .unwrap_or(0);
                size = saved.total.unwrap_or(0);
                if completed > segments.len()
                    || partial.metadata().map(|m| m.len()).unwrap_or(0) < size
                {
                    completed = 0;
                    size = 0;
                }
            }
        }
        let output = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&partial)?;
        output.set_len(size)?;
        drop(output);
        let (mut digest, _) = prefix_hashes(&partial, &self.cancel, false)?;
        let mut output = OpenOptions::new().write(true).open(&partial)?;
        use std::io::Seek;
        output.seek(std::io::SeekFrom::Start(size))?;
        for (index, segment) in segments.iter().enumerate().skip(completed) {
            self.cancel.check()?;
            validate(segment)?;
            let response = fetch(segment)
                .send()
                .await
                .map_err(network_error)
                .and_then(recu_status)?;
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !mime.starts_with("video/mp2t") && !mime.starts_with("application/octet-stream") {
                bail!("El segmento no devolvió MPEG-TS")
            }
            let bytes = response.bytes().await.map_err(network_error)?;
            if bytes.is_empty() {
                bail!("Segmento MPEG-TS vacío")
            }
            output.write_all(&bytes)?;
            output.sync_data()?;
            digest.update(&bytes);
            size += bytes.len() as u64;
            self.state.save_partial(
                &file.path,
                &Partial {
                    source: file.id.clone(),
                    validator: format!("hls:{fingerprint}:{}", index + 1),
                    total: Some(size),
                },
            )?;
            (self.sink)(Event::Progress {
                path: file.path.clone(),
                received: (index + 1) as u64,
                total: Some(segments.len() as u64),
            });
        }
        drop(output);
        if size == 0 {
            bail!("Lista HLS vacía")
        }
        if file.id.starts_with("grabaciones:") {
            return Ok(FileOutcome::Downloaded);
        }
        self.publish(file, &partial, size, &format!("{:x}", digest.finalize()))
    }
    pub async fn download_with_fallback(&self, file: File) -> (File, Result<FileOutcome>) {
        self.download_recu(file).await
    }

    async fn discover(&self, url: &str) -> Result<Listing> {
        let state = self.state.clone();
        let input = url.to_owned();
        let cancel = self.cancel.clone();
        let sink = self.sink.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let log: crate::Log = Arc::new(move |message| sink(Event::Message(message)));
            let mut browser = crate::browser::BrowserSession::launch_dedicated(
                &state.directory.join("browser-profile"),
                false,
                cancel.clone(),
                log.clone(),
            )?;
            sites::recu::resolve(&mut browser, &input, &cancel, &log)
        })
        .await?
    }
    pub async fn run(self: Arc<Self>) -> Result<Summary> {
        self.run_selected(None).await
    }
    fn cached_recu_listing(
        &self,
        url: &str,
        selected: Option<&HashSet<String>>,
    ) -> Result<Option<Listing>> {
        if self.options.site != Site::Recu || self.options.action != Action::Download {
            return Ok(None);
        }
        let Some(selected) = selected else {
            return Ok(None);
        };
        let Some(listing) = self.state.listing(url)? else {
            return Ok(None);
        };
        let user = sites::recu::normalize(url)?
            .rsplit('/')
            .next()
            .unwrap()
            .to_owned();
        let prefix = format!("Recu.me - {user}/");
        if selected
            .iter()
            .filter(|path| path.starts_with(&prefix))
            .all(|path| listing.files.iter().any(|file| &file.path == path))
        {
            return Ok(Some(listing));
        }
        Ok(None)
    }
    pub async fn run_selected(
        self: Arc<Self>,
        selected: Option<Arc<HashSet<String>>>,
    ) -> Result<Summary> {
        let mut summary = Summary::default();
        for (index, url) in self.options.urls.iter().enumerate() {
            self.cancel.check()?;
            (self.sink)(Event::Message(format!(
                "Enlace {}/{}: {url}",
                index + 1,
                self.options.urls.len()
            )));
            let cached = self.cached_recu_listing(url, selected.as_deref())?;
            if cached.is_some() {
                (self.sink)(Event::Message(
                    "Se reutiliza el listado mostrado; se consulta solo cada video seleccionado."
                        .into(),
                ));
            }
            let listing = match if let Some(listing) = cached {
                Ok(listing)
            } else {
                self.discover(url).await
            } {
                Ok(l) => l,
                Err(error) => {
                    self.cancel.check()?;
                    summary.failed += 1;
                    (self.sink)(Event::Message(format!("ERROR: {error}")));
                    continue;
                }
            };

            if self.options.action == Action::List {
                self.state.save_listing(url, &listing)?;
            }
            (self.sink)(Event::Listing {
                url: url.clone(),
                listing: listing.clone(),
            });
            summary.pending += listing.pending.len();
            if self.options.action != Action::Download {
                continue;
            }
            let mut seen = HashMap::new();
            let mut files = Vec::new();
            for file in listing.files {
                if selected
                    .as_ref()
                    .is_some_and(|paths| !paths.contains(&file.path))
                {
                    continue;
                }
                let key = file.path.to_lowercase();
                let source = source_key(&file)?;
                if let Some(previous) = seen.get(&key) {
                    if previous != &source {
                        summary.failed += 1;
                        (self.sink)(Event::FileDone {
                            path: file.path,
                            outcome: FileOutcome::Failed(
                                "Dos archivos distintos usan el mismo destino".into(),
                            ),
                        });
                    }
                    continue;
                }
                seen.insert(key, source);
                files.push(file);
            }
            let mut report = |file: File, outcome: Result<FileOutcome>| -> Result<bool> {
                self.cancel.check()?;
                let browser_lost = outcome.as_ref().err().is_some_and(|error| {
                    error
                        .downcast_ref::<crate::browser::BrowserDisconnected>()
                        .is_some()
                });
                let signin_needed = outcome.as_ref().err().is_some_and(|error| {
                    error
                        .downcast_ref::<sites::recu::SignInRequired>()
                        .is_some()
                });
                if browser_lost && !self.recu_browser_unavailable.swap(true, Ordering::SeqCst) {
                    (self.sink)(Event::Message(
                        "Chrome se cerró dos veces; las grabaciones restantes quedan pendientes para otro intento."
                            .into(),
                    ));
                }
                let outcome = match outcome {
                    Ok(value) => value,
                    Err(error) => {
                        summary.failed += 1;
                        FileOutcome::Failed(error.to_string())
                    }
                };
                match outcome {
                    FileOutcome::Downloaded => summary.downloaded += 1,
                    FileOutcome::Skipped => summary.skipped += 1,
                    _ => {}
                };
                (self.sink)(Event::FileDone {
                    path: file.path,
                    outcome,
                });
                Ok(browser_lost || signin_needed)
            };

            run_recu_queue(
                files,
                |file| {
                    let engine = self.clone();
                    async move { engine.download_with_fallback(file).await }
                },
                &mut report,
            )
            .await?;
        }
        (self.sink)(Event::Finished(summary.clone()));
        Ok(summary)
    }
}

async fn run_recu_queue<T, F, Fut, P>(files: Vec<T>, make_job: F, mut report: P) -> Result<()>
where
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = (T, Result<FileOutcome>)>,
    P: FnMut(T, Result<FileOutcome>) -> Result<bool>,
{
    let capacity = job_limit(Site::Recu, 1, files.len());
    let mut pending = files.into_iter();
    let mut active = stream::FuturesUnordered::new();
    for _ in 0..capacity {
        if let Some(file) = pending.next() {
            active.push(make_job(file));
        }
    }
    let mut stopped = false;
    while let Some((file, outcome)) = active.next().await {
        if report(file, outcome)? {
            stopped = true;
        }
        if !stopped && let Some(next) = pending.next() {
            active.push(make_job(next));
        }
    }
    Ok(())
}

fn recover_recu_browser<T, R>(
    slot: &mut Option<T>,
    mut launch: impl FnMut() -> Result<T>,
    mut source: impl FnMut(&mut T) -> Result<R>,
    mut on_restart: impl FnMut(),
) -> Result<R> {
    for attempt in 0..2 {
        if slot.is_none() {
            *slot = Some(launch()?);
        }
        match source(slot.as_mut().unwrap()) {
            Ok(value) => return Ok(value),
            Err(error)
                if error
                    .downcast_ref::<crate::browser::BrowserDisconnected>()
                    .is_some() =>
            {
                *slot = None;
                if attempt == 0 {
                    on_restart();
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

fn job_limit(_site: Site, _workers: usize, files: usize) -> usize {
    files.clamp(1, 4)
}

fn recu_output_extension(id: &str, source: &str) -> &'static str {
    if id.starts_with("grabaciones:")
        || source
            .split('?')
            .next()
            .is_some_and(|p| p.ends_with(".mp4"))
    {
        "mp4"
    } else {
        "ts"
    }
}
fn recu_stream_end_us(
    ffmpeg: &std::path::Path,
    file: &std::path::Path,
    with_audio: bool,
) -> Result<u64> {
    let mut command = std::process::Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", "0:v:0"]);
    if with_audio {
        command.args(["-map", "0:a:0?"]);
    }
    let output = command
        .args(["-c", "copy", "-f", "null", "-progress", "pipe:1", "-"])
        .output()?;
    if !output.status.success() {
        bail!("FFmpeg no pudo leer por completo el MP4")
    }
    let progress = String::from_utf8(output.stdout)?;
    if !progress.lines().any(|line| line == "progress=end") {
        bail!("FFmpeg no confirmo el final del MP4")
    }
    progress
        .lines()
        .filter_map(|line| {
            line.strip_prefix("out_time_us=")
                .and_then(|value| value.parse::<u64>().ok())
        })
        .next_back()
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow::anyhow!("El MP4 no tiene tiempo de video valido"))
}
fn verify_recu_mp4(ffmpeg: &std::path::Path, file: &std::path::Path) -> Result<()> {
    let video_end = recu_stream_end_us(ffmpeg, file, false)?;
    let media_end = recu_stream_end_us(ffmpeg, file, true)?;
    if media_end.saturating_sub(video_end) > 10_000_000 {
        bail!(
            "El video termina {:.1} s antes que el audio; se conserva la fuente para reintentar",
            (media_end - video_end) as f64 / 1_000_000.0
        )
    }
    Ok(())
}
fn recu_encoder_parallelism() -> usize {
    (std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2)
        / 2)
    .clamp(1, 8)
}

#[cfg(windows)]
fn limit_recu_encoder_process(child: &std::process::Child) -> Result<usize> {
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetProcessAffinityMask(
            process: *mut std::ffi::c_void,
            process_mask: *mut usize,
            system_mask: *mut usize,
        ) -> i32;
        fn SetProcessAffinityMask(process: *mut std::ffi::c_void, mask: usize) -> i32;
    }

    let handle = child.as_raw_handle();
    let mut current = 0usize;
    let mut system = 0usize;
    if unsafe { GetProcessAffinityMask(handle, &mut current, &mut system) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let allowed = current & system;
    if allowed == 0 {
        bail!("FFmpeg no tiene procesadores disponibles");
    }
    let target = ((allowed.count_ones() as usize) / 2).clamp(1, 8);
    let mut selected = 0usize;
    for bit in 0..usize::BITS {
        let flag = 1usize << bit;
        if allowed & flag != 0 {
            selected |= flag;
            if selected.count_ones() as usize == target {
                break;
            }
        }
    }
    if unsafe { SetProcessAffinityMask(handle, selected) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(target)
}
fn recu_ffmpeg_path() -> Result<PathBuf> {
    let local = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Ruta del ejecutable desconocida"))?
        .join("tools")
        .join("ffmpeg.exe");
    if local.is_file() {
        return Ok(local);
    }
    #[cfg(test)]
    {
        let test_binary = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/ffmpeg.exe");
        if test_binary.is_file() {
            return Ok(test_binary);
        }
    }
    let available = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success());
    if available {
        return Ok(PathBuf::from("ffmpeg"));
    }
    bail!(
        "Para guardar grabaciones en MP4 AV1 instala FFmpeg con libsvtav1 o coloca ffmpeg.exe en la carpeta tools junto al descargador"
    )
}
fn recu_status(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(Transient {
            message: format!("Recu.me o Mediafront respondió HTTP {status}"),
            after: None,
        }
        .into());
    }
    Err(crate::transport::HttpFailure(status).into())
}
#[cfg(test)]
fn optional_browser_headers(
    downloading: bool,
    log: &crate::Log,
    collect: impl FnOnce() -> Result<Option<(String, String)>>,
) -> Option<(String, String)> {
    if !downloading {
        return None;
    }
    match collect() {
        Ok(session) => session,
        Err(_) => {
            log("No se pudo recuperar la sesión del navegador; el catálogo se conserva y los archivos bloqueados quedarán pendientes para reintentar.".into());
            None
        }
    }
}
fn prefix_hashes(path: &std::path::Path, cancel: &Cancel, with_md5: bool) -> Result<(Sha256, Md5)> {
    let mut input = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut digest = Sha256::new();
    let mut md5 = Md5::new();
    loop {
        cancel.check()?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        if with_md5 {
            md5.update(&buffer[..n]);
        }
    }
    Ok((digest, md5))
}

#[cfg(test)]
mod browser_export_tests {
    use super::*;
    #[tokio::test]
    async fn recu_browser_failure_stops_queued_files_and_drains_active_work() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let finished = Arc::new(Mutex::new(Vec::new()));
        let started_for_jobs = started.clone();
        let finished_for_results = finished.clone();
        run_recu_queue(
            vec![0usize, 1, 2, 3, 4, 5],
            move |id| {
                let started = started_for_jobs.clone();
                async move {
                    started.lock().unwrap().push(id);
                    if id == 0 {
                        (id, Err(crate::browser::BrowserDisconnected.into()))
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        (id, Ok(FileOutcome::Downloaded))
                    }
                }
            },
            move |id, result| {
                finished_for_results.lock().unwrap().push(id);
                Ok(result.as_ref().err().is_some_and(|error| {
                    error
                        .downcast_ref::<crate::browser::BrowserDisconnected>()
                        .is_some()
                }))
            },
        )
        .await
        .unwrap();
        assert_eq!(*started.lock().unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(*finished.lock().unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn recu_restarts_a_closed_browser_once_and_reuses_the_profile_slot() {
        use crate::browser::BrowserDisconnected;
        let mut slot = Some(1usize);
        let mut launches = 0;
        let mut notices = 0;
        let source = recover_recu_browser(
            &mut slot,
            || {
                launches += 1;
                Ok(2usize)
            },
            |browser| {
                if *browser == 1 {
                    Err(BrowserDisconnected.into())
                } else {
                    Ok(*browser)
                }
            },
            || notices += 1,
        )
        .unwrap();
        assert_eq!(source, 2);
        assert_eq!(slot, Some(2));
        assert_eq!(launches, 1);
        assert_eq!(notices, 1);
    }

    #[cfg(windows)]
    #[test]
    fn recu_encoder_reserves_processors_for_transfers() {
        let child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Start-Sleep -Milliseconds 500; [System.Diagnostics.Process]::GetCurrentProcess().ProcessorAffinity.ToInt64()",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let assigned = limit_recu_encoder_process(&child).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let mask = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .unwrap();
        assert_eq!(mask.count_ones() as usize, assigned);
        assert!(assigned <= (std::thread::available_parallelism().unwrap().get() / 2).max(1));
    }
    #[tokio::test]
    async fn recu_queue_starts_third_transfer_while_two_conversions_are_pending() {
        let conversion = Arc::new(tokio::sync::Semaphore::new(1));
        let release = Arc::new(tokio::sync::Notify::new());
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (converting, mut converting_rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = tokio::spawn(async move {
            run_recu_queue(
                vec![0, 1, 2],
                move |index| {
                    let conversion = conversion.clone();
                    let release = release.clone();
                    let started = started.clone();
                    let converting = converting.clone();
                    async move {
                        started.send(index).unwrap();
                        let _permit = conversion.acquire().await.unwrap();
                        if index == 0 {
                            converting.send(()).unwrap();
                            release.notified().await;
                        }
                        (index, Ok(FileOutcome::Downloaded))
                    }
                },
                |_, _| Ok(false),
            )
            .await
            .unwrap();
        });
        assert_eq!(started_rx.recv().await, Some(0));
        assert_eq!(converting_rx.recv().await, Some(()));
        assert_eq!(started_rx.recv().await, Some(1));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), started_rx.recv())
                .await
                .expect("la tercera transferencia debe iniciar antes de terminar la conversion"),
            Some(2)
        );
        runner.abort();
    }
    #[tokio::test]
    async fn recu_starts_next_transfer_while_previous_conversion_runs() {
        use tokio::sync::Semaphore;
        let transfer = Arc::new(Semaphore::new(1));
        let conversion = Arc::new(Semaphore::new(1));
        let (started, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let (converting, mut conversion_receiver) = tokio::sync::mpsc::unbounded_channel();
        let release_conversion = Arc::new(tokio::sync::Notify::new());
        let release_for_jobs = release_conversion.clone();
        let jobs = stream::iter((0..2).map(move |index| {
            let transfer = transfer.clone();
            let conversion = conversion.clone();
            let started = started.clone();
            let converting = converting.clone();
            let release = release_for_jobs.clone();
            async move {
                let permit = transfer.acquire().await.unwrap();
                started.send(index).unwrap();
                drop(permit);
                let _conversion = conversion.acquire().await.unwrap();
                if index == 0 {
                    converting.send(()).unwrap();
                    release.notified().await;
                }
            }
        }))
        .buffer_unordered(job_limit(Site::Recu, 1, 2));
        let runner = tokio::spawn(async move {
            tokio::pin!(jobs);
            while jobs.next().await.is_some() {}
        });
        assert_eq!(receiver.recv().await, Some(0));
        assert_eq!(conversion_receiver.recv().await, Some(()));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
                .await
                .expect("la siguiente transferencia debe avanzar durante la conversion"),
            Some(1)
        );
        release_conversion.notify_one();
        runner.await.unwrap();
    }
    #[test]
    fn recu_mp4_rejects_video_that_ends_well_before_audio() {
        let ffmpeg = match recu_ffmpeg_path() {
            Ok(path) => path,
            Err(_) => return,
        };
        let root = tempfile::tempdir().unwrap();
        let broken = root.path().join("broken.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-t",
                "1",
                "-i",
                "testsrc2=size=160x90:rate=10",
                "-f",
                "lavfi",
                "-t",
                "15",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&broken)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(verify_recu_mp4(&ffmpeg, &broken).is_err());
        let complete = root.path().join("complete.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-t",
                "2",
                "-i",
                "testsrc2=size=160x90:rate=10",
                "-f",
                "lavfi",
                "-t",
                "2",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&complete)
            .status()
            .unwrap();
        assert!(status.success());
        verify_recu_mp4(&ffmpeg, &complete).unwrap();
    }

    #[tokio::test]
    async fn recu_transcodes_a_transport_stream_before_publishing_mp4() {
        let ffmpeg = match recu_ffmpeg_path() {
            Ok(path) => path,
            Err(_) => return,
        };
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("fixture.part");
        let generated = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-f",
                "mpegts",
                "-y",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(generated.success());
        let url = "https://es.recu.me/performer/fixture_performer_000";
        let engine = Engine::new(
            RunOptions {
                site: Site::Recu,
                urls: vec![url.into()],
                root: root.path().to_path_buf(),
                action: Action::Download,
                workers: 1,
                password: None,
            },
            Cancel::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        let file = File {
            id: "grabaciones:1".into(),
            post: url.into(),
            url: "https://es.recu.me/fixture_performer_000/video/1/play".into(),
            path: "Recu.me - fixture_performer_000/grabaciones/1.mp4".into(),
            ..Default::default()
        };
        let outcome = engine
            .finish_recu_recording(&file, input.clone(), false)
            .await
            .unwrap();
        assert!(matches!(outcome, FileOutcome::Downloaded));
        assert!(input.is_file());
        let target = engine.state.target(&file.path).unwrap();
        assert!(target.is_file());
        assert!(engine.state.completed(&file, &Cancel::default()).unwrap());
        let inspected = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-i"])
            .arg(&target)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&inspected.stderr).contains("Video: av1"));
        let decoded = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&target)
            .args(["-map", "0:v:0", "-frames:v", "1", "-f", "null", "-"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(decoded.success());
    }
    #[test]
    fn recu_recordings_target_mp4_even_when_stream_is_hls() {
        assert_eq!(
            recu_output_extension(
                "grabaciones:900000001",
                "https://f41.mediafront.net/vod/u/a/index.m3u8"
            ),
            "mp4"
        );
        assert_eq!(
            recu_output_extension(
                "clips:900011",
                "https://f41.mediafront.net/vod/u/a/index.m3u8"
            ),
            "ts"
        );
        assert_eq!(
            recu_output_extension(
                "clips:900011",
                "https://f41.mediafront.net/vod/u/a/clip.mp4"
            ),
            "mp4"
        );
    }
    #[test]
    fn recu_selected_download_reuses_the_saved_catalog() {
        let root = tempfile::tempdir().unwrap();
        let url = "https://es.recu.me/performer/fixture_performer_000";
        let engine = Engine::new(
            RunOptions {
                site: Site::Recu,
                urls: vec![url.into()],
                root: root.path().to_path_buf(),
                action: Action::Download,
                workers: 1,
                password: None,
            },
            Cancel::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        let listing = Listing {
            title: "cached catalog".into(),
            files: vec![File {
                id: "grabaciones:900000001".into(),
                post: url.into(),
                url: "https://es.recu.me/fixture_performer_000/video/900000001/play".into(),
                path: "Recu.me - fixture_performer_000/grabaciones/900000001.ts".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        engine.state.save_listing(url, &listing).unwrap();
        let selected = HashSet::from([listing.files[0].path.clone()]);
        assert_eq!(
            engine
                .cached_recu_listing(url, Some(&selected))
                .unwrap()
                .unwrap()
                .title,
            "cached catalog"
        );
        assert!(engine.cached_recu_listing(url, None).unwrap().is_none());
        let two_profiles = HashSet::from([
            listing.files[0].path.clone(),
            "Recu.me - other/grabaciones/2.mp4".into(),
        ]);
        assert!(
            engine
                .cached_recu_listing(url, Some(&two_profiles))
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .cached_recu_listing(
                    url,
                    Some(&HashSet::from([
                        "Recu.me - fixture_performer_000/grabaciones/missing.ts".into()
                    ]))
                )
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn listing_never_requires_cookie_export_and_closed_browser_keeps_the_result() {
        let log: crate::Log = Arc::new(|_| {});
        assert!(
            optional_browser_headers(false, &log, || panic!(
                "list must not contact a closed browser"
            ))
            .is_none()
        );
        assert!(
            optional_browser_headers(true, &log, || anyhow::bail!("disconnected browser"))
                .is_none()
        );
        assert_eq!(
            optional_browser_headers(true, &log, || Ok(Some((
                "session=fixture".into(),
                "browser".into()
            )))),
            Some(("session=fixture".into(), "browser".into()))
        );
    }
}
