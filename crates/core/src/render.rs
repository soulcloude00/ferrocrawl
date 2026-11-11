//! The slow path: headless Chromium, started on first use and shared by every request.
//!
//! Memory is the constraint on small machines, so:
//! - every tab is closed when its render ends, timeouts included (a dropped render future used to
//!   leave its tab open, and tabs piled up over a long crawl);
//! - Chromium moves itself into its own systemd scope (`app-org.chromium.Chromium-<pid>.scope`,
//!   with no memory limit) right after it starts; we put a cap on that scope, so a heavy page
//!   can only take Chromium down, never the app or the terminal it runs in;
//! - a browser that died (killed at its cap, crashed) is replaced on the next render.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Page;
use futures::StreamExt;
use tokio::sync::{Mutex, Semaphore};

const NAV_CAP: Duration = Duration::from_secs(8);
const CHALLENGE_CAP: Duration = Duration::from_secs(4);
/// How often the render loop reads the page's text.
const POLL: Duration = Duration::from_millis(150);
/// How long a page with almost no text gets to fill in before we stop waiting.
const SHORT_WAIT: Duration = Duration::from_secs(6);
/// Chromium's own cap (`FERROCRAWL_CHROME_MEM` overrides).
const CHROME_MEM: &str = "1200M";

fn is_waiting_screen(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    crate::extract::is_challenge(&lower) || lower.contains("verification successful") || lower.contains("checking your browser")
}

pub struct Renderer {
    browser: Mutex<Option<Arc<Browser>>>,
    tabs: Semaphore,
}

pub struct Rendered {
    pub url: String,
    pub html: String,
}

impl Renderer {
    pub fn new(max_tabs: usize) -> Self {
        Self { browser: Mutex::new(None), tabs: Semaphore::new(max_tabs.max(1)) }
    }

    async fn browser(&self) -> Result<Arc<Browser>> {
        let mut slot = self.browser.lock().await;
        if let Some(b) = slot.as_ref() {
            return Ok(b.clone());
        }
        // A profile of our own, so two ferrocrawl processes (the app and the CLI) never fight
        // over one, and leftovers of a process that died are cleaned up first.
        reap_stale_profiles();
        let profile = profile_dir(std::process::id());
        let builder = BrowserConfig::builder()
            .user_data_dir(&profile)
            .no_sandbox()
            // chromiumoxide adds the leading `--` itself.
            .arg("disable-gpu")
            .arg("disable-dev-shm-usage")
            .arg("disable-extensions")
            .arg("mute-audio")
            .arg("disable-background-networking")
            .arg(("blink-settings", "imagesEnabled=false"))
            // Few renderer processes and a capped V8 heap.
            .arg(("renderer-process-limit", "3"))
            .arg(("js-flags", "--max-old-space-size=384"));
        let config = builder.build().map_err(|e| anyhow!("browser config: {e}"))?;
        let (mut browser, mut handler) = Browser::launch(config).await.context("launching Chromium (is it installed?)")?;
        if let Some(pid) = browser.get_mut_child().and_then(|c| c.inner.id()) {
            cap_memory(pid);
        }
        tokio::spawn(async move { while handler.next().await.is_some() {} });
        let b = Arc::new(browser);
        *slot = Some(b.clone());
        Ok(b)
    }

    /// Forgets a browser that stopped answering, so the next render starts a fresh one.
    async fn reset(&self, dead: &Arc<Browser>) {
        let mut slot = self.browser.lock().await;
        if slot.as_ref().is_some_and(|b| Arc::ptr_eq(b, dead)) {
            *slot = None;
        }
    }

    pub async fn render(&self, url: &str, wait_for: Duration, timeout: Duration) -> Result<Rendered> {
        self.render_within(url, wait_for, None, timeout).await
    }

    /// Like [`render`](Self::render), but gives up when no tab frees up within `tab_wait`:
    /// a fallback render isn't worth queueing behind other pages for.
    pub async fn render_within(&self, url: &str, wait_for: Duration, tab_wait: Option<Duration>, timeout: Duration) -> Result<Rendered> {
        let _tab = match tab_wait {
            Some(w) => tokio::time::timeout(w, self.tabs.acquire()).await.map_err(|_| anyhow!("browser busy"))??,
            None => self.tabs.acquire().await?,
        };
        let browser = self.browser().await?;
        let page = match tokio::time::timeout(Duration::from_secs(10), browser.new_page("about:blank")).await {
            Ok(Ok(p)) => p,
            // Can't even open a tab: the browser is gone. Start over next time.
            Ok(Err(e)) => {
                self.reset(&browser).await;
                return Err(anyhow!(e).context("opening a tab"));
            }
            Err(_) => {
                self.reset(&browser).await;
                return Err(anyhow!("browser not responding"));
            }
        };
        block_heavy(&page).await;
        let result = tokio::time::timeout(timeout, load(&page, url, wait_for)).await;
        // Close the tab whatever happened; a leaked tab keeps its renderer's memory.
        let _ = tokio::time::timeout(Duration::from_secs(3), page.close()).await;
        result.map_err(|_| anyhow!("render timed out after {timeout:?}"))?
    }
}

/// Requests a render never needs: fonts, audio and video, and the ad and analytics networks
/// that keep a page busy long after its content is in. (Images are off for the whole browser.)
const BLOCKED: &[&str] = &[
    "*://*:*/*.woff2", "*://*:*/*.woff", "*://*:*/*.ttf", "*://*:*/*.otf", "*://*:*/*.eot",
    "*://*:*/*.mp4", "*://*:*/*.webm", "*://*:*/*.mp3", "*://*:*/*.m4a", "*://*:*/*.ogg", "*://*:*/*.m3u8",
    "*://*.doubleclick.net:*/*", "*://*.googlesyndication.com:*/*", "*://*.googletagservices.com:*/*",
    "*://*.google-analytics.com:*/*", "*://*.googletagmanager.com:*/*", "*://*.adservice.google.com:*/*",
    "*://*.amazon-adsystem.com:*/*", "*://*.adnxs.com:*/*", "*://*.criteo.com:*/*", "*://*.criteo.net:*/*",
    "*://*.taboola.com:*/*", "*://*.outbrain.com:*/*", "*://*.scorecardresearch.com:*/*", "*://*.quantserve.com:*/*",
    "*://*.hotjar.com:*/*", "*://*.clarity.ms:*/*", "*://*.facebook.net:*/*", "*://*.moatads.com:*/*",
    "*://*.rubiconproject.com:*/*", "*://*.pubmatic.com:*/*", "*://*.casalemedia.com:*/*", "*://*.openx.net:*/*",
    "*://*.chartbeat.com:*/*", "*://*.newrelic.com:*/*", "*://*.nr-data.net:*/*", "*://*.segment.io:*/*",
    "*://*.mixpanel.com:*/*", "*://*.fullstory.com:*/*", "*://*.yandex.ru:*/metrika/*", "*://mc.yandex.ru:*/*",
];

async fn block_heavy(page: &Page) {
    use chromiumoxide::cdp::browser_protocol::network::{BlockPattern, SetBlockedUrLsParams};
    let params = SetBlockedUrLsParams { url_patterns: Some(BLOCKED.iter().map(|p| BlockPattern::new(*p, true)).collect()) };
    if let Err(e) = tokio::time::timeout(Duration::from_secs(2), page.execute(params)).await.map_err(anyhow::Error::from).and_then(|r| r.map_err(anyhow::Error::from)) {
        tracing::debug!("blocking heavy requests failed: {e:#}");
    }
}

/// Text lengths this close count as unchanged: a carousel or a live clock shouldn't keep a
/// finished page waiting until the cap.
fn about_same(a: usize, b: usize) -> bool {
    a.abs_diff(b) <= (b / 100).max(8)
}

async fn load(page: &Page, url: &str, wait_for: Duration) -> Result<Rendered> {
    // Don't wait for the `load` event: ad-heavy pages take a minute to fire it, and app shells
    // fire it long before their content arrives. Watch the page's text instead and stop as soon
    // as it has settled.
    let nav = async {
        let _ = page.goto(url).await;
    };
    tokio::pin!(nav);
    let start = std::time::Instant::now();
    let (mut nav_done, mut last, mut stable) = (false, 0usize, 0u32);
    loop {
        tokio::select! {
            _ = &mut nav, if !nav_done => nav_done = true,
            _ = tokio::time::sleep(POLL) => {}
        }
        let len = text_len(page).await;
        stable = if len > 0 && about_same(len, last) { stable + 1 } else { 0 };
        last = len;
        let waited = start.elapsed();
        let settled = if len < 200 {
            // Short text is either a shell still loading its content (Salesforce sites sit on a
            // spinner for seconds) or a final screen: a block or error page. Stop early only for
            // the second; a shell that stays short ends as an empty-page failure anyway.
            nav_done && stable >= 2 && (waited > SHORT_WAIT || final_screen(page).await)
        } else {
            // Settled: unchanged for ~0.3 s after the load finished, or ~0.45 s before it.
            (nav_done && stable >= 2) || stable >= 3
        };
        // Still blank after a few seconds: nothing is coming.
        let blank = len < 50 && waited > SHORT_WAIT;
        if settled || blank || waited > NAV_CAP {
            break;
        }
    }
    if !wait_for.is_zero() {
        tokio::time::sleep(wait_for).await;
    }
    let mut html = page.content().await?;
    // A challenge screen ("Just a moment…", "Verification successful. Waiting for…") usually
    // clears by itself in a few seconds; poll until it does.
    let t0 = std::time::Instant::now();
    while t0.elapsed() < CHALLENGE_CAP && is_waiting_screen(&html) {
        tokio::time::sleep(Duration::from_millis(400)).await;
        html = page.content().await?;
    }
    let final_url = page.url().await?.unwrap_or_else(|| url.to_string());
    Ok(Rendered { url: final_url, html })
}

/// A short page that is finished as it is: a block, challenge or error screen.
async fn final_screen(page: &Page) -> bool {
    let js = "document.body ? document.body.innerText.slice(0, 2000) : ''";
    let text = match tokio::time::timeout(Duration::from_millis(800), page.evaluate(js)).await {
        Ok(Ok(v)) => v.into_value::<String>().unwrap_or_default(),
        _ => return false,
    };
    let lower = text.to_lowercase();
    crate::extract::is_challenge(&lower) || matches!(crate::extract::failure_page(&text), Some(w) if w != "empty page")
}

/// Length of the page's visible text; 0 while it can't be read (mid-navigation).
async fn text_len(page: &Page) -> usize {
    let js = "document.body ? document.body.innerText.length : 0";
    match tokio::time::timeout(Duration::from_millis(800), page.evaluate(js)).await {
        Ok(Ok(v)) => v.into_value::<usize>().unwrap_or(0),
        _ => 0,
    }
}

fn profile_dir(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ferrocrawl-chrome-{pid}"))
}

/// Stops Chromium instances whose ferrocrawl died without closing them (killed, crashed) and
/// removes their profiles. Matches on the profile path, so no other browser is touched.
fn reap_stale_profiles() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("ferrocrawl-chrome-").and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid == std::process::id() || std::path::Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        kill_by_profile(&e.path());
        let _ = std::fs::remove_dir_all(e.path());
    }
}

/// SIGTERMs every process whose command line names `profile` as its user-data-dir.
fn kill_by_profile(profile: &std::path::Path) {
    let needle = format!("--user-data-dir={}", profile.display());
    let Ok(procs) = std::fs::read_dir("/proc") else { return };
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(cmd) = std::fs::read(p.path().join("cmdline")) else { continue };
        if String::from_utf8_lossy(&cmd).split('\0').any(|a| a == needle) {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Don't leave a browser behind when the engine goes away.
        let profile = profile_dir(std::process::id());
        if profile.exists() {
            kill_by_profile(&profile);
            let _ = std::fs::remove_dir_all(&profile);
        }
    }
}

/// Caps the memory of the systemd scope Chromium puts itself in (`FERROCRAWL_CHROME_MEM`,
/// default 1.2 GB; `FERROCRAWL_CHROME_MEM=0` leaves it alone). Chromium creates the scope a moment
/// after starting, so this waits for it. Only a scope Chromium made is touched, never the one
/// the app or a terminal runs in.
fn cap_memory(pid: u32) {
    let mem = std::env::var("FERROCRAWL_CHROME_MEM").unwrap_or_else(|_| CHROME_MEM.into());
    if mem == "0" {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            let Ok(cg) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else { return };
            let unit = cg.trim().rsplit('/').next().unwrap_or_default().to_string();
            if unit.ends_with(".scope") && unit.to_ascii_lowercase().contains("chromium") {
                let ok = std::process::Command::new("systemctl")
                    .args(["--user", "set-property", "--runtime", &unit, &format!("MemoryMax={mem}"), "MemorySwapMax=1G"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                tracing::debug!("capped {unit} at {mem}: {ok}");
                return;
            }
        }
    });
}
