//! Recu.me catalog and signed HLS validation.
use anyhow::{Context, Result, bail};
use scraper::{Html, Selector};
use std::collections::HashSet;
use url::Url;

const HOST: &str = "es.recu.me";
fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}
fn secure(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(HOST)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}
pub fn normalize(input: &str) -> Result<String> {
    let url = Url::parse(input)?;
    if !secure(&url) || url.query().is_some() {
        bail!("Perfil de Recu.me no válido")
    }
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if parts.len() != 2 || parts[0] != "performer" || !slug(parts[1]) {
        bail!("Perfil de Recu.me no válido")
    }
    Ok(format!("https://{HOST}/performer/{}", parts[1]))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Recordings,
    Clips,
}
impl Kind {
    fn path(self, user: &str) -> String {
        match self {
            Self::Recordings => format!("/performer/{user}/latest"),
            Self::Clips => format!("/clips/{user}"),
        }
    }
    fn selector(self) -> &'static str {
        match self {
            Self::Recordings => ".performer-page-videos .video-thumb[data-id]",
            Self::Clips => ".performer-page-clips .video-thumb[data-id]",
        }
    }
    fn item_path(self, user: &str, id: &str) -> String {
        match self {
            Self::Recordings => format!("/{user}/video/{id}/play"),
            Self::Clips => format!("/clips/{id}/play"),
        }
    }
}
pub fn catalog_url(profile: &str, kind: Kind, page: usize) -> Result<String> {
    let user = normalize(profile)?.rsplit('/').next().unwrap().to_owned();
    if page == 0 || page > 1000 {
        bail!("Página de Recu.me no válida")
    }
    let suffix = if page == 1 {
        String::new()
    } else {
        format!("/page/{page}")
    };
    Ok(format!("https://{HOST}{}{suffix}", kind.path(&user)))
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogCard {
    pub id: String,
    pub url: String,
    pub thumbnail: Option<String>,
    pub published_at: Option<String>,
}
pub fn parse_catalog(
    html: &str,
    profile: &str,
    kind: Kind,
    page: usize,
) -> Result<(Vec<(String, String)>, usize)> {
    let (cards, pages) = parse_catalog_cards(html, profile, kind, page)?;
    Ok((
        cards.into_iter().map(|card| (card.id, card.url)).collect(),
        pages,
    ))
}
fn valid_thumbnail(input: &str, user: &str) -> bool {
    let Ok(url) = Url::parse(input) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    let server = host
        .strip_prefix('f')
        .and_then(|s| s.strip_suffix(".mediafront.net"));
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().split('/').any(|part| part == user)
        && url.path().ends_with(".jpg")
        && server
            .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
}
pub fn parse_catalog_cards(
    html: &str,
    profile: &str,
    kind: Kind,
    page: usize,
) -> Result<(Vec<CatalogCard>, usize)> {
    let expected = catalog_url(profile, kind, page)?;
    let user = normalize(profile)?.rsplit('/').next().unwrap().to_owned();
    let doc = Html::parse_document(html);
    if let Some(canonical) = doc
        .select(&Selector::parse("link[rel=canonical]").unwrap())
        .next()
        .and_then(|e| e.value().attr("href"))
    {
        if canonical.trim_end_matches('/') != expected {
            bail!("El catálogo pertenece a otra página")
        }
    } else {
        let owner = format!("/performer/{user}");
        if !doc
            .select(&Selector::parse("h1 a.performer-link[href]").unwrap())
            .any(|e| e.value().attr("href") == Some(owner.as_str()))
        {
            bail!("No se identificó el perfil del catálogo")
        }
    }
    let selector = Selector::parse(kind.selector()).unwrap();
    let link_selector = Selector::parse("a[href]").unwrap();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for element in doc.select(&selector) {
        if element.value().attr("data-performer") != Some(user.as_str()) {
            continue;
        }
        let Some(id) = element.value().attr("data-id") else {
            continue;
        };
        if id.parse::<u64>().ok().filter(|n| *n > 0).is_none() || !seen.insert(id.to_owned()) {
            continue;
        }
        let path = kind.item_path(&user, id);
        if !element.select(&link_selector).any(|a| {
            a.value()
                .attr("href")
                .is_some_and(|href| href.split('?').next() == Some(path.as_str()))
        }) {
            bail!("Elemento del catálogo sin enlace esperado")
        }
        let thumbnail = element
            .select(&Selector::parse(".video-splash[data-src]").unwrap())
            .next()
            .and_then(|e| e.value().attr("data-src"))
            .filter(|url| valid_thumbnail(url, &user))
            .map(str::to_owned);
        let published_at = element
            .select(&Selector::parse(".video-date").unwrap())
            .next()
            .map(|e| e.text().collect::<String>().trim().to_owned())
            .filter(|text| {
                text.len() >= 16 && text.as_bytes()[4] == b'-' && text.as_bytes()[7] == b'-'
            });
        out.push(CatalogCard {
            id: id.to_owned(),
            url: format!("https://{HOST}{path}"),
            thumbnail,
            published_at,
        });
    }
    if out.is_empty() {
        bail!("El catálogo no declaró archivos")
    }
    let pages = doc
        .select(&Selector::parse("ul.pagination[data-pg-count]").unwrap())
        .next()
        .and_then(|e| e.value().attr("data-pg-count"))
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(1);
    if pages < page || pages > 1000 {
        bail!("Paginación de Recu.me no válida")
    }
    Ok((out, pages))
}
pub fn parse_player_json(value: &serde_json::Value) -> Result<String> {
    if value["reason"] == "shall_signin" {
        bail!("Recu.me nego la reproduccion: shall_signin")
    }
    if value["status"] != true {
        bail!(
            "Recu.me negó la reproducción: {}",
            value["reason"].as_str().unwrap_or("acceso no disponible")
        )
    }
    let html = value["player"]
        .as_str()
        .context("El reproductor no devolvió HTML")?;
    let doc = Html::parse_fragment(html);
    let source = doc
        .select(&Selector::parse("video source[src], video[src]").unwrap())
        .next()
        .and_then(|e| e.value().attr("src"))
        .context("El reproductor no declaró una fuente")?;
    if !source.starts_with("https://") {
        bail!("Fuente del reproductor no válida")
    }
    Ok(source.to_owned())
}

pub fn validate_hls(input: &str, profile: &str) -> Result<Url> {
    let user = normalize(profile)?.rsplit('/').next().unwrap().to_owned();
    let url = Url::parse(input)?;
    let host = url.host_str().context("Fuente HLS sin servidor")?;
    let server = host
        .strip_prefix('f')
        .and_then(|s| s.strip_suffix(".mediafront.net"));
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !server
            .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
        || parts.len() != 4
        || parts[0] != "vod"
        || parts[1] != user
        || !parts[2]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b','))
        || !(matches!(
            parts[3],
            "index.m3u8" | "max.m3u8" | "240p.m3u8" | "360p.m3u8" | "480p.m3u8" | "720p.m3u8"
        ) || (parts[3].starts_with("seg-")
            && parts[3].ends_with(".ts")
            && parts[3][4..parts[3].len() - 3]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'v' || b == b'a')))
    {
        bail!("La fuente HLS no pertenece al perfil")
    }
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
    for key in [
        "uid",
        "mid",
        "origin",
        "request_id",
        "p",
        "expires",
        "md5",
        "secret",
    ] {
        if !pairs
            .get(key)
            .is_some_and(|v| !v.is_empty() && v.len() < 150)
        {
            bail!("Firma HLS incompleta")
        }
    }
    if pairs["expires"].parse::<u64>().is_err() {
        bail!("Caducidad HLS no válida")
    }
    Ok(url)
}
pub fn segment_check(url: &Url) -> Result<String> {
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
    let uid = pairs.get("uid").context("Falta uid HLS")?;
    let request = pairs.get("request_id").context("Falta request_id HLS")?;
    let expires = pairs.get("expires").context("Falta caducidad HLS")?;
    if uid.len() < 6
        || request.len() < 4
        || expires.len() < 4
        || !uid.is_ascii()
        || !request.is_ascii()
        || !expires.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("Firma HLS no válida")
    }
    Ok(format!(
        "{}{}{}",
        &request[..4],
        &uid[2..6],
        &expires[expires.len() - 4..]
    ))
}
pub fn parse_master(html: &str, master: &str, profile: &str) -> Result<String> {
    parse_master_for(html, master, profile, validate_hls)
}
pub fn parse_clip_master(html: &str, master: &str, profile: &str, id: &str) -> Result<String> {
    parse_master_for(html, master, profile, |url, profile| {
        validate_clip_hls(url, profile, id)
    })
}
fn parse_master_for(
    html: &str,
    master: &str,
    profile: &str,
    validate: impl Fn(&str, &str) -> Result<Url>,
) -> Result<String> {
    let base = validate(master, profile)?;
    if !html.starts_with("#EXTM3U") || !html.contains("#EXT-X-STREAM-INF:") {
        bail!("Lista maestra HLS no válida")
    }
    let mut lines = html.lines();
    let mut chosen: Option<(u64, String)> = None;
    while let Some(line) = lines.next() {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let variant = lines.next().context("Falta variante HLS")?.trim();
            if variant.starts_with('#') {
                bail!("Variante HLS no válida")
            }
            let url = base.join(variant)?;
            validate(url.as_str(), profile)?;
            if url.path().rsplit_once('/').map(|v| v.0) != base.path().rsplit_once('/').map(|v| v.0)
            {
                bail!("Variante HLS fuera de la grabación")
            }
            let bandwidth = line
                .split(',')
                .find_map(|part| {
                    part.trim()
                        .strip_prefix("BANDWIDTH=")
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .unwrap_or(0);
            let rank = if line.contains("NAME=max") || variant.starts_with("max.m3u8") {
                u64::MAX
            } else {
                bandwidth
            };
            if chosen.as_ref().is_none_or(|(best, _)| rank > *best) {
                chosen = Some((rank, url.to_string()));
            }
        }
    }
    chosen
        .map(|(_, url)| url)
        .context("Falta variante HLS declarada")
}
pub fn parse_segments(html: &str, variant: &str, profile: &str) -> Result<Vec<String>> {
    parse_segments_for(html, variant, profile, validate_hls)
}
pub fn parse_clip_segments(
    html: &str,
    variant: &str,
    profile: &str,
    id: &str,
) -> Result<Vec<String>> {
    parse_segments_for(html, variant, profile, |url, profile| {
        validate_clip_hls(url, profile, id)
    })
}
fn parse_segments_for(
    html: &str,
    variant: &str,
    profile: &str,
    validate: impl Fn(&str, &str) -> Result<Url>,
) -> Result<Vec<String>> {
    let base = validate(variant, profile)?;
    if !html.starts_with("#EXTM3U")
        || !html.contains("#EXT-X-ENDLIST")
        || html.contains("#EXT-X-KEY")
        || html.contains("#EXT-X-BYTERANGE")
    {
        bail!("Lista HLS no compatible")
    }
    let mut segments = Vec::new();
    for line in html
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
    {
        let mut url = base.join(line)?;
        validate(url.as_str(), profile)?;
        if url.path().rsplit_once('/').map(|v| v.0) != base.path().rsplit_once('/').map(|v| v.0) {
            bail!("Segmento HLS fuera de la grabación")
        }
        if !url
            .path()
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("seg-") && name.ends_with(".ts"))
        {
            bail!("Segmento HLS no válido")
        }
        let check = segment_check(&url)?;
        if url.query_pairs().any(|(k, _)| k == "check") {
            bail!("Segmento HLS con check inesperado")
        }
        url.query_pairs_mut().append_pair("check", &check);
        segments.push(url.to_string());
        if segments.len() > 100000 {
            bail!("Demasiados segmentos HLS")
        }
    }
    if segments.is_empty() {
        bail!("Lista HLS vacía")
    }
    Ok(segments)
}

pub fn validate_player_page(input: &str, profile: &str, kind: Kind, id: &str) -> Result<()> {
    let user = normalize(profile)?.rsplit('/').next().unwrap().to_owned();
    if id.parse::<u64>().ok().filter(|n| *n > 0).is_none() {
        bail!("Identificador de Recu.me no válido")
    }
    let url = Url::parse(input)?;
    if !secure(&url) || url.query().is_some() || url.path() != kind.item_path(&user, id) {
        bail!("El reproductor no pertenece al perfil")
    }
    Ok(())
}

pub fn parse_clip_json(value: &serde_json::Value) -> Result<String> {
    if let Some(reason) = value.as_str() {
        bail!("Recu.me negó el clip: {reason}")
    }
    let source = value["clip"]["videoUrl"]
        .as_str()
        .context("El clip no devolvió videoUrl")?;
    if !source.starts_with("https://") {
        bail!("Fuente del clip no válida")
    }
    Ok(source.to_owned())
}

pub fn validate_clip_source(input: &str, profile: &str, id: &str) -> Result<Url> {
    normalize(profile)?;
    let url = Url::parse(input)?;
    let host = url.host_str().unwrap_or("");
    let server = host
        .strip_prefix('f')
        .and_then(|s| s.strip_suffix(".mediafront.net"));
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !server
            .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
        || !url.path().split('/').any(|part| {
            part.starts_with(&format!("clip-{id}."))
                || part.starts_with(&format!("clip-{id}-"))
                || part == format!("clip-{id}")
        })
        || !(url.path().ends_with(".mp4")
            || url.path().ends_with(".m3u8")
            || url.path().ends_with(".ts"))
    {
        bail!("La fuente del clip no pertenece al perfil")
    }
    Ok(url)
}

pub fn validate_clip_hls(input: &str, profile: &str, id: &str) -> Result<Url> {
    let url = validate_clip_source(input, profile, id)?;
    if !(url.path().ends_with(".m3u8") || url.path().ends_with(".ts")) {
        bail!("La fuente del clip no es HLS")
    }
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
    for key in [
        "uid",
        "mid",
        "origin",
        "request_id",
        "p",
        "expires",
        "md5",
        "secret",
    ] {
        if !pairs
            .get(key)
            .is_some_and(|v| !v.is_empty() && v.len() < 150)
        {
            bail!("Firma HLS del clip incompleta")
        }
    }
    if pairs["expires"].parse::<u64>().is_err() {
        bail!("Caducidad HLS del clip no válida")
    }
    Ok(url)
}

pub fn resolve(
    browser: &mut crate::browser::BrowserSession,
    profile: &str,
    cancel: &crate::Cancel,
    log: &crate::Log,
) -> Result<crate::Listing> {
    let profile = normalize(profile)?;
    let user = profile.rsplit('/').next().unwrap();
    let mut listing = crate::Listing {
        title: format!("Recu.me - {user}"),
        ..Default::default()
    };
    let mut preview_user_agent = None;
    for kind in [Kind::Recordings, Kind::Clips] {
        let mut page = 1;
        loop {
            cancel.check()?;
            let address = catalog_url(&profile, kind, page)?;
            log(format!("Leyendo {address}"));
            browser.navigate(&address)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            let (items, pages) = loop {
                cancel.check()?;
                let html = browser.evaluate("document.documentElement.outerHTML")?;
                if let Some(html) = html.as_str()
                    && let Ok(result) = parse_catalog_cards(html, &profile, kind, page)
                {
                    break result;
                }
                if std::time::Instant::now() >= deadline {
                    bail!("No cargó el catálogo de Recu.me; comprueba la ventana de Chrome")
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            };
            if preview_user_agent.is_none() {
                let answer = browser.evaluate("navigator.userAgent")?;
                let agent = answer
                    .as_str()
                    .filter(|value| {
                        value.len() <= 512 && value.is_ascii() && value.starts_with("Mozilla/5.0")
                    })
                    .context("User-Agent del navegador no disponible")?;
                preview_user_agent = Some(agent.to_owned());
            }
            for card in items {
                let (category, extension) = match kind {
                    Kind::Recordings => ("grabaciones", "mp4"),
                    Kind::Clips => ("clips", "ts"),
                };
                listing.files.push(crate::File {
                    id: format!("{category}:{}", card.id),
                    post: profile.clone(),
                    url: card.url,
                    path: format!("Recu.me - {user}/{category}/{}.{extension}", card.id),
                    thumbnail: card.thumbnail,
                    preview_user_agent: preview_user_agent.clone(),
                    published_at: card.published_at,
                    ..Default::default()
                });
            }
            if page >= pages {
                break;
            }
            page += 1;
        }
    }
    sort_recent(&mut listing.files);
    Ok(listing)
}
pub fn sort_recent(files: &mut [crate::File]) {
    files.sort_by(|a, b| {
        let a_recording = a.id.starts_with("grabaciones:");
        let b_recording = b.id.starts_with("grabaciones:");
        b_recording
            .cmp(&a_recording)
            .then_with(|| b.published_at.cmp(&a.published_at))
            .then_with(|| {
                let id = |file: &crate::File| {
                    file.id
                        .rsplit(':')
                        .next()
                        .unwrap_or("0")
                        .parse::<u64>()
                        .unwrap_or(0)
                };
                id(b).cmp(&id(a))
            })
    });
}

#[derive(Debug)]
pub struct SignInRequired;
impl std::fmt::Display for SignInRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Inicia sesión en Chrome para acceder a Recu.me y vuelve a descargar"
        )
    }
}
impl std::error::Error for SignInRequired {}
pub fn browser_source(
    browser: &mut crate::browser::BrowserSession,
    file: &crate::File,
    cancel: &crate::Cancel,
) -> Result<(String, String)> {
    let (kind, id) = if let Some(id) = file.id.strip_prefix("grabaciones:") {
        (Kind::Recordings, id)
    } else if let Some(id) = file.id.strip_prefix("clips:") {
        (Kind::Clips, id)
    } else {
        bail!("Archivo de Recu.me no válido")
    };
    let user = normalize(&file.post)?
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let page = format!("https://{HOST}{}", kind.item_path(&user, id));
    if file.url != page {
        if kind == Kind::Clips {
            validate_clip_source(&file.url, &file.post, id)?;
        } else {
            validate_hls(&file.url, &file.post)?;
        }
    }
    let navigation = if kind == Kind::Clips {
        catalog_url(&file.post, Kind::Clips, 1)?
    } else {
        page
    };
    browser.navigate(&navigation)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut signin_required = false;
    loop {
        cancel.check()?;
        let script = match kind {
            Kind::Recordings => format!(
                r#"(async()=>{{let r=await fetch('/api/video/{id}?parent-request='+encodeURIComponent(window.requestId||''),{{credentials:'same-origin',headers:{{'X-Api-Token':window.token||'','X-Requested-With':'XMLHttpRequest','Accept':'application/json'}}}});return {{status:r.status,body:await r.text(),ua:navigator.userAgent}}}})()"#
            ),
            Kind::Clips => format!(
                r#"(async()=>{{let r=await fetch('/clips/{id}/player?json=1',{{credentials:'same-origin',headers:{{'X-Api-Token':window.token||'','X-Requested-With':'XMLHttpRequest','Accept':'application/json'}}}});return {{status:r.status,body:await r.text(),ua:navigator.userAgent}}}})()"#
            ),
        };
        let answer = match browser.evaluate(&script) {
            Ok(answer) => Some(answer),
            Err(error)
                if error
                    .downcast_ref::<crate::browser::BrowserDisconnected>()
                    .is_some() =>
            {
                return Err(error);
            }
            Err(_) => None,
        };
        if let Some(answer) = answer {
            let status = answer["status"].as_u64().unwrap_or(0);
            let body = answer["body"].as_str().unwrap_or("");
            let signin_response = body.contains("shall_signin");
            if signin_response {
                signin_required = true;
            }
            if status == 200 && !signin_response {
                let source = match kind {
                    Kind::Recordings => serde_json::from_str(body)
                        .ok()
                        .and_then(|v| parse_player_json(&v).ok()),
                    Kind::Clips => serde_json::from_str(body)
                        .ok()
                        .and_then(|v| parse_clip_json(&v).ok()),
                };
                if let Some(source) = source {
                    match kind {
                        Kind::Recordings => {
                            if let Err(error) = validate_hls(&source, &file.post) {
                                let safe_path = Url::parse(&source)
                                    .map(|url| {
                                        format!("{}{}", url.host_str().unwrap_or(""), url.path())
                                    })
                                    .unwrap_or_else(|_| "URL invalida".to_owned());
                                bail!("{error}: {safe_path}");
                            }
                        }
                        Kind::Clips => {
                            validate_clip_source(&source, &file.post, id)?;
                        }
                    }
                    let ua = answer["ua"]
                        .as_str()
                        .context("Falta User-Agent del navegador")?;
                    return Ok((source, ua.to_owned()));
                }
                if body.contains("shall_subscribe")
                    || body.contains("views_restricted")
                    || body.contains("daily_limit")
                {
                    bail!("Recu.me restringió la reproducción de este archivo")
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            if signin_required {
                return Err(SignInRequired.into());
            }
            bail!(
                "No se obtuvo la fuente; inicia sesión o resuelve el acceso en la ventana de Chrome"
            )
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

pub fn validate_media(url: &Url, profile: &str) -> Result<()> {
    let user = normalize(profile)?.rsplit('/').next().unwrap().to_owned();
    if secure(url) && url.query().is_none() {
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        if (parts.len() == 4
            && parts[0] == user
            && parts[1] == "video"
            && parts[3] == "play"
            && parts[2].parse::<u64>().is_ok())
            || (parts.len() == 3
                && parts[0] == "clips"
                && parts[2] == "play"
                && parts[1].parse::<u64>().is_ok())
        {
            return Ok(());
        }
    }
    for part in url.path().split('/') {
        if let Some(rest) = part.strip_prefix("clip-") {
            let id: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            if !id.is_empty() && validate_clip_source(url.as_str(), profile, &id).is_ok() {
                return Ok(());
            }
        }
    }
    if validate_hls(url.as_str(), profile).is_ok() {
        return Ok(());
    }
    bail!("URL de Recu.me fuera del perfil")
}

#[cfg(test)]
mod player_response_tests {
    use super::*;
    #[test]
    fn signin_response_does_not_expose_a_preview_as_the_recording() {
        let body = serde_json::json!({
            "status": true,
            "reason": "shall_signin",
            "player": "<video><source src=\"https://f42.mediafront.net/vod/other/2000-01-01,00-00/index.m3u8\"></video>"
        });
        assert!(parse_player_json(&body).is_err());
    }
}
