use anyhow::{Context, Result};
use arboard::Clipboard;
use regex::Regex;
use serde::Deserialize;
use std::{fs, collections::{HashSet, VecDeque}, path::{Path, PathBuf}, time::Duration};
use tokio::{process::Command, sync::mpsc};
use tokio::fs::create_dir_all;
use url::Url;
use reqwest::header::{CONTENT_DISPOSITION, HeaderMap};

#[derive(Debug, Deserialize)]
struct RdSection {
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MonitorSection {
    poll_interval_ms: Option<u64>,
    hosts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DownloadSection {
    dir: String,
    aria2c_path: String,
    aria2c_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AppConfig {
    realdebrid: RdSection,
    monitor: MonitorSection,
    download: DownloadSection,
}

impl AppConfig {
    fn hosts_set(&self) -> HashSet<String> {
        self.monitor
            .hosts
            .iter()
            .map(|h| h.to_lowercase())
            .collect()
    }

    fn poll_ms(&self) -> u64 {
        self.monitor.poll_interval_ms.unwrap_or(700)
    }

    fn rd_token(&self) -> Result<String> {
        // ENV overrides file value
        if let Ok(envv) = std::env::var("REALDEBRID_TOKEN") {
            if !envv.trim().is_empty() {
                return Ok(envv);
            }
        }
        self.realdebrid
            .token
            .clone()
            .filter(|s| !s.trim().is_empty())
            .context("RealDebrid token missing (set in rd_clip.toml or REALDEBRID_TOKEN)")
    }
}

async fn rd_health_check(token: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = "https://api.real-debrid.com/rest/1.0/user";

    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("RD health check request failed")?;

    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        anyhow::bail!(
            "RD health check failed: HTTP {} – {} (is your token correct / not expired?)",
            status,
            txt
        );
    }
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or_default();
    let username = v.get("username").and_then(|x| x.as_str()).unwrap_or("unknown");
    let email    = v.get("email").and_then(|x| x.as_str()).unwrap_or("hidden");
    println!("[startup] Real-Debrid token OK — user: {username}, email: {email}");
    Ok(())

}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg_path = std::env::var("RD_CLIP_CONFIG").unwrap_or_else(|_| "rddown.toml".to_string());
    let cfg = load_config(&cfg_path)?;
    create_dir_all(&cfg.download.dir).await.ok();

    // preflight: ensure aria2c exists
    ensure_aria2c(&cfg.download.aria2c_path).await?;

    let hosts = cfg.hosts_set();
    let poll_ms = cfg.poll_ms();
    let rd_token = cfg.rd_token()?;

    // ping realdebrid
    rd_health_check(&rd_token).await?;

    let (tx, mut rx) = mpsc::channel::<String>(64);

    // worker
    let dl_cfg = cfg.download;
    let worker_token = rd_token.clone();
    tokio::spawn(async move {
        while let Some(uri) = rx.recv().await {
            if let Err(e) = process_uri(&worker_token, &dl_cfg, &uri).await {
                eprintln!("[worker] {} -> error: {:#}", uri, e);
            }
        }
    });

    // monitor
    let url_regex = Regex::new(r#"(?xi)\b(https?://[^\s'\"<>]+)"#).unwrap();
    let mut last_seen = String::new();

    loop {
        let mut cb = match Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[monitor] clipboard open failed: {e:?}");
                tokio::time::sleep(Duration::from_millis(1000)).await;
                continue;
            }
        };

        if let Ok(text) = cb.get_text() {
            if text != last_seen {
                last_seen = text.clone();
                for cap in url_regex.captures_iter(&text) {
                    let candidate = cap.get(1).unwrap().as_str().to_string();
                    if let Ok(parsed) = Url::parse(&candidate) {
                        if matches_host(&parsed, &hosts) {
                            println!("[monitor] matched host: {}", parsed.host_str().unwrap_or(""));
                            if let Err(e) = tx.try_send(candidate.clone()) {
                                // channel full → await
                                if let Err(e2) = tx.send(candidate.clone()).await {
                                    eprintln!("[monitor] send failed: {e2:?}");
                                }
                            }
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
}

fn matches_host(u: &Url, hosts: &HashSet<String>) -> bool {
    if let Some(h) = u.host_str() {
        let h = h.to_lowercase();
        hosts.contains(&h) || hosts.iter().any(|base| h.ends_with(&format!(".{}", base)))
    } else {
        false
    }
}

async fn process_uri(token: &str, dl: &DownloadSection, uri: &str) -> Result<()> {
    println!("[worker] processing: {uri}");
    let resp = rd_unrestrict_link(token, uri).await?;
    let links = extract_download_links(&resp)?;
    if links.is_empty() {
        println!("[worker] no downloadable links returned");
        return Ok(());
    }
    // Fire each through aria2c (sequentially; change to join_all for parallel)
    for link in links {
        aria2c_download(&dl.aria2c_path, dl.aria2c_args.as_deref(), &dl.dir, &link).await?;
    }
    Ok(())
}

async fn rd_unrestrict_link(token: &str, link: &str) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let url = "https://api.real-debrid.com/rest/1.0/unrestrict/link";
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("link={}", urlencoding::encode(link)))
        .send()
        .await
        .context("RD request failed")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("RD error {}: {}", status, text);
    }
    Ok(serde_json::from_str(&text).context("parse RD JSON")?)
}

fn extract_download_links(v: &serde_json::Value) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(s) = v.get("download").and_then(|s| s.as_str()) {
        out.push(s.to_string());
    }
    if let Some(arr) = v.get("links").and_then(|l| l.as_array()) {
        for it in arr {
            if let Some(s) = it.as_str() {
                out.push(s.to_string());
            }
        }
    }
    // Some responses come as arrays of objects with "download"/"link"
    if v.is_array() {
        for it in v.as_array().unwrap() {
            if let Some(s) = it.get("download").and_then(|s| s.as_str()) {
                out.push(s.to_string());
            } else if let Some(s) = it.get("link").and_then(|s| s.as_str()) {
                out.push(s.to_string());
            }
        }
    }
    Ok(out)
}

async fn ensure_aria2c(path: &str) -> Result<()> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    let status = cmd.status().await;
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => anyhow::bail!("aria2c not found or not executable at '{}'", path),
    }
}

async fn aria2c_download(
    aria2c_path: &str,
    extra_args: Option<&[String]>,
    dir: &str,
    url: &str,
) -> Result<()> {
    // Try to detect filename before handing to aria2c
    let filename = detect_filename(url).await.unwrap_or_else(|| "download.bin".into());
    let _out_path = format!("{}/{}", dir.trim_end_matches('/'), filename);

    // Build command: aria2c --dir <dir> --out <filename> [args...] <url>
    let mut args: Vec<String> = vec![
        "--dir".into(),
        dir.into(),
        "--out".into(),
        filename.clone(),
    ];
    if let Some(more) = extra_args {
        args.extend(more.iter().cloned());
    }
    args.push(url.into());

    println!("[aria2c] {} {}", aria2c_path, args.join(" "));
    let status = Command::new(aria2c_path).args(&args).status().await?;
    if !status.success() {
        anyhow::bail!("aria2c failed with status {status}");
    }
    Ok(())
}

async fn detect_filename(url: &str) -> Option<String> {
    let client = reqwest::Client::new();

    // HEAD first; if disallowed, fall back to GET with range=0-0
    let resp = match client.head(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => {
            client
                .get(url)
                .header("Range", "bytes=0-0")
                .send()
                .await
                .ok()?
        }
    };

    let headers = resp.headers();
    extract_filename_from_headers(headers)
        .or_else(|| filename_from_url(url))
}

fn extract_filename_from_headers(headers: &HeaderMap) -> Option<String> {
    headers.get(CONTENT_DISPOSITION).and_then(|cd| {
        let val = cd.to_str().ok()?;
        // Typical forms: attachment; filename="file.zip" or inline; filename*=UTF-8''file.zip
        if let Some(pos) = val.find("filename=") {
            let fname = &val[pos + 9..];
            let cleaned = fname.trim_matches(|c| c == '"' || c == '\'' || c == ';');
            Some(cleaned.to_string())
        } else if let Some(pos) = val.find("filename*=") {
            let fname = &val[pos + 10..];
            let cleaned = fname.split("''").last()?.trim_matches('"');
            Some(cleaned.to_string())
        } else {
            None
        }
    })
}

fn filename_from_url(u: &str) -> Option<String> {
    Url::parse(u)
        .ok()?
        .path_segments()?
        .last()
        .map(|s| s.to_string())
}

fn load_config(path: &str) -> Result<AppConfig> {
    let cfg_path = expand_tilde(path);
    let text = fs::read_to_string(&cfg_path)
        .with_context(|| format!("failed to read config file at '{}'", cfg_path.display()))?;

    let cfg: AppConfig = toml::from_str(&text)
        .with_context(|| format!("TOML parse error in '{}'", cfg_path.display()))?;

    // Basic validation / sane defaults are handled by AppConfig methods,
    // but we can catch obvious misconfigurations here too.
    if cfg.monitor.hosts.is_empty() {
        anyhow::bail!("config: [monitor].hosts must contain at least one host");
    }
    if cfg.download.dir.trim().is_empty() {
        anyhow::bail!("config: [download].dir must not be empty");
    }
    if cfg.download.aria2c_path.trim().is_empty() {
        anyhow::bail!("config: [download].aria2c_path must not be empty");
    }

    Ok(cfg)
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(stripped) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    Path::new(p).to_path_buf()
}

struct Dedupe {
    seen: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}
impl Dedupe {
    fn new(cap: usize) -> Self {
        Self { seen: HashSet::new(), order: VecDeque::new(), cap }
    }
    fn insert_or_seen(&mut self, s: &str) -> bool {
        if self.seen.contains(s) { return true; }
        self.seen.insert(s.to_string());
        self.order.push_back(s.to_string());
        if self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        false
    }
}
