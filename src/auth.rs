use anyhow::{Context, Result, anyhow};
use chromiumoxide::{Browser, BrowserConfig, cdp::browser_protocol::network::DeleteCookiesParams};
use futures::StreamExt;
use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};
use tokio::{sync::Mutex, task::JoinHandle};
use url::Url;

const SESSION_COOKIE: &str = "d2lSessionVal";
const SECURE_SESSION_COOKIE: &str = "d2lSecureSessionVal";

pub type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Authentication boundary used by the Brightspace HTTP client.
pub trait SessionProvider: Send + Sync {
    fn cookie(&self, force_login: bool) -> SessionFuture<'_, Result<String>>;
    fn clear(&self) -> SessionFuture<'_, ()>;
    fn shutdown(&self) -> SessionFuture<'_, Result<()>>;
}

/// Keeps Microsoft and Brightspace cookies in Chromium's persistent profile.
/// Brightspace cookies stay in memory only and are reacquired from that profile
/// after a server restart.
pub struct BrowserSession {
    base_url: Url,
    profile_dir: PathBuf,
    cookie: Mutex<Option<String>>,
    browser: Mutex<Option<BrowserLifetime>>,
}

struct BrowserLifetime {
    browser: Browser,
    _handler_task: JoinHandle<()>,
}

impl BrowserSession {
    pub fn new(base_url: Url, profile_dir: PathBuf) -> Self {
        Self {
            base_url,
            profile_dir,
            cookie: Mutex::new(None),
            browser: Mutex::new(None),
        }
    }

    pub async fn cookie(&self, force_login: bool) -> Result<String> {
        let mut cached = self.cookie.lock().await;
        if !force_login {
            if let Some(cookie) = cached.as_ref() {
                return Ok(cookie.clone());
            }
        }
        let cookie = self.login(force_login).await?;
        *cached = Some(cookie.clone());
        Ok(cookie)
    }

    pub async fn clear(&self) {
        *self.cookie.lock().await = None;
    }

    async fn login(&self, force_login: bool) -> Result<String> {
        std::fs::create_dir_all(&self.profile_dir).with_context(|| {
            format!(
                "cannot create browser profile at {}",
                self.profile_dir.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.profile_dir, std::fs::Permissions::from_mode(0o700))?;
        }

        eprintln!(
            "Brightspace session expired. Opening browser for Microsoft sign-in and Authenticator approval…"
        );
        let mut builder = BrowserConfig::builder()
            .with_head()
            .user_data_dir(&self.profile_dir);
        if let Some(path) = std::env::var_os("BRIGHTSPACE_CHROME_PATH") {
            builder = builder.chrome_executable(path);
        }
        let config = builder
            .build()
            .map_err(anyhow::Error::msg)
            .context("could not configure the visible Chromium browser")?;
        let login_url = self.base_url.join("d2l/login")?;
        let mut browser = self.browser.lock().await;
        if browser.is_none() {
            let (browser_instance, mut handler) = Browser::launch(config).await.context(
                "could not launch Chromium; install Chrome/Chromium or set BRIGHTSPACE_CHROME_PATH",
            )?;
            let handler_task = tokio::spawn(async move {
                while let Some(event) = handler.next().await {
                    if event.is_err() {
                        break;
                    }
                }
            });
            *browser = Some(BrowserLifetime {
                browser: browser_instance,
                _handler_task: handler_task,
            });
        }
        let browser = browser
            .as_ref()
            .ok_or_else(|| anyhow!("Chromium browser failed to initialize"))?;
        let page = browser.browser.new_page("about:blank").await?;
        if force_login {
            // Chromium restores Brightspace session cookies the server already
            // rejected; delete them so only cookies from a fresh sign-in qualify.
            page.delete_cookies(
                [SESSION_COOKIE, SECURE_SESSION_COOKIE]
                    .map(|name| DeleteCookiesParams {
                        url: Some(self.base_url.to_string()),
                        ..DeleteCookiesParams::new(name)
                    })
                    .into(),
            )
            .await?;
        }
        page.goto(login_url.as_str()).await?;
        page.bring_to_front().await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        let mut cookie_header = None;
        while tokio::time::Instant::now() < deadline {
            let cookies = browser.browser.get_cookies().await?;
            let host = self.base_url.host_str().unwrap_or_default();
            let on_brightspace = |cookie: &&chromiumoxide::cdp::browser_protocol::network::Cookie| {
                let domain = cookie.domain.trim_start_matches('.');
                host == domain || host.ends_with(&format!(".{domain}"))
            };
            let session = cookies
                .iter()
                .filter(on_brightspace)
                .find(|cookie| cookie.name == SESSION_COOKIE);
            let secure = cookies
                .iter()
                .filter(on_brightspace)
                .find(|cookie| cookie.name == SECURE_SESSION_COOKIE);
            if let (Some(session), Some(secure)) = (session, secure) {
                cookie_header = Some(format!(
                    "{}={}; {}={}",
                    session.name, session.value, secure.name, secure.value
                ));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        cookie_header.ok_or_else(|| {
            anyhow!(
                "Microsoft sign-in did not reach Brightspace within 10 minutes; retry the request"
            )
        })
    }
}

impl SessionProvider for BrowserSession {
    fn cookie(&self, force_login: bool) -> SessionFuture<'_, Result<String>> {
        Box::pin(BrowserSession::cookie(self, force_login))
    }

    fn clear(&self) -> SessionFuture<'_, ()> {
        Box::pin(BrowserSession::clear(self))
    }

    fn shutdown(&self) -> SessionFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(mut lifetime) = self.browser.lock().await.take() {
                lifetime.browser.close().await?;
                lifetime
                    ._handler_task
                    .await
                    .context("Chromium event handler failed during shutdown")?;
            }
            Ok(())
        })
    }
}
