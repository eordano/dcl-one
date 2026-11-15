//! Outbound mail: the confirmation email of the set-email flow and the
//! `email` delivery channel of the outbox worker (rendered per type by
//! `templates`), both SendGrid dynamic templates over the ONE outbound client (SendGrid and Turnstile are fixed
//! public hosts, so the guard costs nothing). Unconfigured means refused
//! loudly for the confirmation (the caller answers 500) and held `pending` for a
//! delivery row: never a silent success.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use parking_lot::RwLock;
use serde_json::{Value as Json, json};
use uuid::Uuid;

use super::errors::ApiError;
use super::{signing, templates};
use crate::explore::api::feature_flags::Snapshot;
use crate::http::Outbound;
use crate::jobs::{Jobs, PeriodicCfg};

pub const CODE_LEN: usize = 32;
const TURNSTILE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// A held disposable list is fetched again after this, as upstream.
const DISPOSABLE_REFRESH: Duration = Duration::from_secs(3600);
/// The refresh job's tick: an empty list is retried this soon.
const DISPOSABLE_RETRY: Duration = Duration::from_secs(60);
const DISPOSABLE_TIMEOUT: Duration = Duration::from_secs(30);
const DISPOSABLE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// `DISPOSABLE_EMAIL_DOMAINS_MIN`'s default: the upstream list holds thousands, so fewer is a broken download.
pub const DISPOSABLE_MIN_DOMAINS: usize = 1000;
/// The `dapps` flag that turns the confirm-email challenge on.
pub const TURNSTILE_FLAG: &str = "turnstile-verification";
/// The `dapps` flag whose variant lists refused email domains.
pub const BLACKLISTED_DOMAINS_FLAG: &str = "credits-blacklisted-domains";

/// The node's `dapps` flags (the communities mirror); empty, every flag is off.
#[derive(Clone)]
pub struct DappsFlags(pub Arc<Snapshot>);

impl Default for DappsFlags {
    fn default() -> Self {
        Self(Arc::new(Snapshot::empty()))
    }
}

impl std::fmt::Debug for DappsFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DappsFlags")
    }
}

/// 32 hex characters (a v4 uuid: 122 random bits); the old crate drew 32
/// alphanumerics from `rand`, which v2 does not otherwise depend on. The
/// confirm route checks length and equality only.
pub fn make_code() -> String {
    Uuid::new_v4().simple().to_string()
}

#[derive(Clone, Debug, Default)]
pub struct EmailConfig {
    pub sendgrid_api_key: Option<String>,
    /// `SENDGRID_API_URL`; empty means no SendGrid.
    pub api_url: String,
    /// `DISPOSABLE_EMAIL_DOMAINS_URL`; empty fetches no list.
    pub disposable_list_url: String,
    /// `DISPOSABLE_EMAIL_DOMAINS_MIN`: a download with fewer domains keeps the previous list.
    pub disposable_min_domains: usize,
    pub from_email: Option<String>,
    /// The generic dynamic template (old `SENDGRID_EMAIL_TEMPLATE_ID`): the
    /// outbox's `email` channel, the confirmation and the ad-hoc email; unset,
    /// every delivery row stays pending.
    pub notification_template_id: Option<String>,
    /// `SENDGRID_VALIDATE_EMAIL_TEMPLATE_ID`: the account confirmation's own template, when set.
    pub validate_email_template_id: Option<String>,
    /// `SENDGRID_VALIDATE_CREDITS_EMAIL_TEMPLATE_ID`: the credits confirmation's own template, when set.
    pub validate_credits_email_template_id: Option<String>,
    pub account_base_url: String,
    pub turnstile_secret_key: Option<String>,
    pub domain_blacklist: Vec<String>,
    /// `SENDGRID_SANDBOX_MODE`: SendGrid validates the generic-template mail but delivers none.
    pub sandbox_mode: bool,
    /// `ENV`: the `environment` custom arg of a notification email.
    pub environment: String,
    /// `turnstile-verification` and `credits-blacklisted-domains`.
    pub dapps_flags: DappsFlags,
}

impl EmailConfig {
    /// `EMAIL_DOMAIN_BLACKLIST`: comma-separated, lowercased, blanks dropped.
    pub fn parse_domain_blacklist(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailSource {
    Account,
    Credits,
}

impl EmailSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EmailSource::Account => "account",
            EmailSource::Credits => "credits",
        }
    }

    pub fn from_credits_workflow(is_credits: bool) -> Self {
        if is_credits {
            EmailSource::Credits
        } else {
            EmailSource::Account
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "credits" => EmailSource::Credits,
            _ => EmailSource::Account,
        }
    }
}

/// Why a delivery send did not happen; decides retry vs terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendFailure {
    /// Nothing to send with: no key, sender or template.
    NotConfigured,
    /// No `SIGNING_KEY` for the unsubscribe links: held until one is set.
    NoSigningKey,
    /// Transport or 5xx/429: retry later.
    Transient(String),
    /// The provider refused the message (4xx): retrying cannot help.
    Permanent(String),
}

/// Where a notification email's signed unsubscribe links point.
#[derive(Clone, Debug)]
struct Unsubscribe {
    signing_key: String,
    base_url: String,
}

#[derive(Clone)]
pub struct EmailSender {
    cfg: EmailConfig,
    http: Outbound,
    unsubscribe: Option<Unsubscribe>,
    /// The last disposable-domain list fetched; shared by every clone.
    disposable: Arc<RwLock<Disposable>>,
}

#[derive(Default)]
struct Disposable {
    domains: HashSet<String>,
    fetched_at: Option<Instant>,
}

impl EmailSender {
    pub fn new(cfg: EmailConfig, http: Outbound) -> Self {
        Self {
            cfg,
            http,
            unsubscribe: None,
            disposable: Arc::default(),
        }
    }

    /// `SIGNING_KEY` and `NOTIFICATIONS_SERVICE_BASE_URL`; without a key no notification email goes out.
    pub fn with_unsubscribe(mut self, signing_key: Option<&str>, base_url: &str) -> Self {
        self.unsubscribe = signing_key.filter(|k| !k.is_empty()).map(|k| Unsubscribe {
            signing_key: k.to_string(),
            base_url: base_url.to_string(),
        });
        self
    }

    /// Key, sender and the notification template are set; a missing
    /// signing key holds deliveries pending instead ([`Self::has_signing_key`]).
    pub fn notifications_configured(&self) -> bool {
        self.html_configured() && self.cfg.notification_template_id.is_some()
    }

    pub fn has_signing_key(&self) -> bool {
        self.unsubscribe.is_some()
    }

    /// The old domain validator's rules: more than one dot, or a leading or
    /// trailing dot, in the local part; a disposable domain; a domain in the
    /// `credits-blacklisted-domains` flag variant or in `EMAIL_DOMAIN_BLACKLIST`.
    pub fn is_domain_blacklisted(&self, email: &str) -> bool {
        let Some((local, domain)) = email.split_once('@') else {
            return true;
        };
        if domain.contains('@')
            || local.matches('.').count() > 1
            || local.starts_with('.')
            || local.ends_with('.')
        {
            return true;
        }
        let domain = domain.to_lowercase();
        if self.disposable.read().domains.contains(&domain) {
            return true;
        }
        let flagged = self
            .cfg
            .dapps_flags
            .0
            .variant_values("dapps", BLACKLISTED_DOMAINS_FLAG)
            .unwrap_or_default();
        flagged
            .iter()
            .chain(&self.cfg.domain_blacklist)
            .any(|d| d == &domain)
    }

    /// Replaces the disposable list from `url` when it holds at least `min_domains`; a failed
    /// or implausible fetch keeps the last list (an empty one refuses nothing, as upstream).
    pub async fn refresh_disposable_domains(
        &self,
        url: &str,
        min_domains: usize,
    ) -> anyhow::Result<usize> {
        let body = tokio::time::timeout(DISPOSABLE_TIMEOUT, async {
            let mut resp = self.http.get(url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!("disposable email domains: {status}");
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if body.len() + chunk.len() > DISPOSABLE_MAX_BYTES {
                    anyhow::bail!("disposable email domains: over {DISPOSABLE_MAX_BYTES} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
        .await
        .map_err(|_| anyhow::anyhow!("disposable email domains: timed out"))??;
        let domains: HashSet<String> = String::from_utf8_lossy(&body)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_lowercase)
            .collect();
        let count = domains.len();
        if count < min_domains {
            anyhow::bail!(
                "disposable email domains: {count} domains, expected at least {min_domains}"
            );
        }
        *self.disposable.write() = Disposable {
            domains,
            fetched_at: Some(Instant::now()),
        };
        Ok(count)
    }

    /// No list yet, or the held one is an hour old.
    fn disposable_due(&self) -> bool {
        self.disposable
            .read()
            .fetched_at
            .is_none_or(|at| at.elapsed() >= DISPOSABLE_REFRESH)
    }

    /// Every process keeps its own list: fetched at start, retried each minute while
    /// missing, then refreshed hourly. `false` (no job) when no list URL is set.
    pub fn spawn_disposable_refresh(&self, jobs: &Jobs) -> bool {
        let url = self.cfg.disposable_list_url.clone();
        if url.is_empty() {
            return false;
        }
        let sender = self.clone();
        let min = self.cfg.disposable_min_domains;
        // Warn on the first failure, debug while it lasts, info on recovery.
        let failing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        jobs.spawn_periodic(
            "notification-disposable-domains",
            PeriodicCfg::every(DISPOSABLE_RETRY),
            move |_ctx| {
                let (sender, url, failing) = (sender.clone(), url.clone(), failing.clone());
                async move {
                    if !sender.disposable_due() {
                        return Ok(());
                    }
                    let result = sender.refresh_disposable_domains(&url, min).await;
                    let was_failing =
                        failing.swap(result.is_err(), std::sync::atomic::Ordering::Relaxed);
                    match (result, was_failing) {
                        (Ok(count), true) => tracing::info!(count, "disposable email domains recovered"),
                        (Ok(count), false) => tracing::debug!(count, "disposable email domains refreshed"),
                        (Err(error), false) => tracing::warn!(%error, "disposable email domains refresh failed; keeping the last list"),
                        (Err(error), true) => tracing::debug!(%error, "disposable email domains still failing"),
                    }
                    Ok(())
                }
            },
        );
        true
    }

    /// The challenge's secret while the `turnstile-verification` flag is on; `None` turns the check off.
    pub fn turnstile_secret(&self) -> Option<&str> {
        self.cfg
            .turnstile_secret_key
            .as_deref()
            .filter(|_| self.cfg.dapps_flags.0.enabled("dapps", TURNSTILE_FLAG))
    }

    /// The account site's page the confirmation links to, as the old inbox built it:
    /// the challenge page while Turnstile is on, else the source's own confirm page.
    pub fn confirm_url(&self, source: EmailSource, address: &str, code: &str) -> String {
        let base = self.cfg.account_base_url.trim_end_matches('/');
        match (self.turnstile_secret().is_some(), source) {
            (true, _) => format!(
                "{base}/confirm-email-challenge/{code}?address={address}&source={}",
                source.as_str()
            ),
            (false, EmailSource::Credits) => {
                format!("{base}/credits-email-confirmed/{code}?address={address}")
            }
            (false, EmailSource::Account) => {
                format!("{base}/confirm-email/{code}?address={address}")
            }
        }
    }

    fn send_url(&self) -> String {
        format!("{}/v3/mail/send", self.cfg.api_url.trim_end_matches('/'))
    }

    async fn sendgrid(&self, api_key: &str, body: &Json) -> Result<Option<String>, SendFailure> {
        let resp = self
            .http
            .post(self.send_url())
            .header(AUTHORIZATION, format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| SendFailure::Transient(format!("sendgrid request failed: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            let message_id = resp
                .headers()
                .get("x-message-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            return Ok(message_id);
        }
        let detail = resp.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        tracing::error!(status = %status, detail = %detail, "SendGrid send failed");
        if status.as_u16() == 429 || status.is_server_error() {
            Err(SendFailure::Transient(format!("sendgrid {status}")))
        } else {
            Err(SendFailure::Permanent(format!(
                "sendgrid {status}: {detail}"
            )))
        }
    }

    /// The source's own `SENDGRID_VALIDATE_*` template when set (and `true`), else the generic one.
    fn confirmation_template(&self, source: EmailSource) -> (Option<&str>, bool) {
        let own = match source {
            EmailSource::Account => self.cfg.validate_email_template_id.as_deref(),
            EmailSource::Credits => self.cfg.validate_credits_email_template_id.as_deref(),
        };
        match own {
            Some(id) => (Some(id), true),
            None => (self.cfg.notification_template_id.as_deref(), false),
        }
    }

    /// The old inbox's validate-email / validate-credits-email body through
    /// the generic template.
    pub async fn send_confirmation(
        &self,
        source: EmailSource,
        to: &str,
        address: &str,
        code: &str,
        user_name: Option<&str>,
    ) -> Result<Option<String>, ApiError> {
        let link = self.confirm_url(source, address, code);
        let mut data = validate_email_data(source, &link, user_name);
        let (template, own) = self.confirmation_template(source);
        if own {
            // The dedicated templates' own fields.
            data["confirmUrl"] = json!(link);
            data["code"] = json!(code);
            data["source"] = json!(source.as_str());
        }
        match self
            .send_with_template(
                template,
                to,
                data,
                json!({ "email_type": "validation_attempt" }),
            )
            .await
        {
            Ok(message_id) => {
                tracing::info!(
                    source = source.as_str(),
                    ?message_id,
                    "confirmation email sent"
                );
                Ok(message_id)
            }
            Err(SendFailure::Transient(e)) => Err(ApiError::Internal(e)),
            Err(SendFailure::Permanent(_)) => Err(ApiError::Internal(
                "failed to send confirmation email".into(),
            )),
            Err(SendFailure::NotConfigured | SendFailure::NoSigningKey) => {
                tracing::error!(
                    source = source.as_str(),
                    "SendGrid not configured; cannot send confirmation email"
                );
                Err(ApiError::Internal(
                    "email delivery is not configured".into(),
                ))
            }
        }
    }

    /// One message through the generic template with the old sendgrid-client's
    /// `dynamic_template_data` (`address` is the recipient) and `custom_args`
    /// (`environment` and a fresh `tracking_id` unless given).
    pub async fn send_generic(
        &self,
        to: &str,
        data: Json,
        custom_args: Json,
    ) -> Result<Option<String>, SendFailure> {
        self.send_with_template(
            self.cfg.notification_template_id.as_deref(),
            to,
            data,
            custom_args,
        )
        .await
    }

    async fn send_with_template(
        &self,
        template_id: Option<&str>,
        to: &str,
        data: Json,
        custom_args: Json,
    ) -> Result<Option<String>, SendFailure> {
        let (Some(api_key), Some(from), Some(template_id), true) = (
            self.cfg.sendgrid_api_key.as_deref(),
            self.cfg.from_email.as_deref(),
            template_id,
            self.html_configured(),
        ) else {
            return Err(SendFailure::NotConfigured);
        };
        let body = self.template_body(from, template_id, to, data, custom_args);
        self.sendgrid(api_key, &body).await
    }

    /// The old sendgrid-client's body; `SENDGRID_SANDBOX_MODE` rides here only.
    fn template_body(
        &self,
        from: &str,
        template_id: &str,
        to: &str,
        mut data: Json,
        mut custom_args: Json,
    ) -> Json {
        data["address"] = json!(to);
        custom_args["environment"] = json!(self.cfg.environment);
        if custom_args.get("tracking_id").is_none() {
            custom_args["tracking_id"] = json!(Uuid::new_v4().to_string());
        }
        json!({
            "personalizations": [{ "to": [{ "email": to }], "dynamic_template_data": data }],
            "from": { "email": from, "name": "Decentraland" },
            "template_id": template_id,
            "custom_args": custom_args,
            "mail_settings": { "sandbox_mode": { "enable": self.cfg.sandbox_mode } },
        })
    }

    /// Key, sender and API URL are set: a code-rendered message can go out.
    pub fn html_configured(&self) -> bool {
        self.cfg.sendgrid_api_key.is_some()
            && self.cfg.from_email.is_some()
            && !self.cfg.api_url.trim().is_empty()
    }

    /// `TURNSTILE_SECRET_KEY` is set but the flag is off, so confirm-email runs no challenge.
    pub fn turnstile_secret_without_flag(&self) -> bool {
        self.cfg.turnstile_secret_key.is_some() && self.turnstile_secret().is_none()
    }

    /// One code-rendered message (a subject, an HTML body and its text
    /// twin, the Shop's availability alert) through the same key and
    /// sender; `custom_args` ride on the personalization so a redelivery
    /// is recognisable. The provider's message id when it gives one.
    pub async fn send_html(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: &str,
        custom_args: &Json,
    ) -> Result<Option<String>, SendFailure> {
        let (Some(api_key), Some(from), true) = (
            self.cfg.sendgrid_api_key.as_deref(),
            self.cfg.from_email.as_deref(),
            self.html_configured(),
        ) else {
            return Err(SendFailure::NotConfigured);
        };
        let body = json!({
            "personalizations": [{ "to": [{ "email": to }], "custom_args": custom_args }],
            "from": { "email": from },
            "subject": subject,
            "content": [
                { "type": "text/plain", "value": text },
                { "type": "text/html", "value": html },
            ],
        });
        self.sendgrid(api_key, &body).await
    }

    /// The old processor's `dynamic_template_data`: the type's rendered
    /// fields, signed unsubscribe links and the account link. `None` when the
    /// type has no template.
    pub fn notification_data(
        &self,
        to: &str,
        address: &str,
        kind: &str,
        metadata: &Json,
    ) -> Result<Option<Json>, SendFailure> {
        let Some(unsubscribe) = &self.unsubscribe else {
            return Err(SendFailure::NoSigningKey);
        };
        let Some(rendered) =
            templates::render(kind, address, metadata).map_err(SendFailure::Permanent)?
        else {
            return Ok(None);
        };
        // `new URL(path, base)`: the absolute path replaces any base path.
        let sign = |path: String| {
            url::Url::parse(&unsubscribe.base_url)
                .and_then(|base| base.join(&path))
                .ok()
                .and_then(|u| signing::sign_url(&unsubscribe.signing_key, u.as_str()))
                .ok_or_else(|| SendFailure::Permanent(format!("unsignable unsubscribe url {path}")))
        };
        let mut data = serde_json::Map::new();
        data.insert(
            "subject".into(),
            rendered.fields.get("subject").cloned().unwrap_or_default(),
        );
        data.insert("address".into(), json!(to));
        data.insert("content".into(), json!(rendered.content));
        // Absent fields stay absent, as `JSON.stringify` dropped them.
        for key in [
            "actionButtonLink",
            "actionButtonText",
            "title",
            "titleHighlight",
            "bannerUrl",
            "bannerLabel",
        ] {
            if let Some(v) = rendered.fields.get(key) {
                data.insert(key.into(), v.clone());
            }
        }
        // `titleHighlight` goes raw in every email: the template's `{{ }}` escapes it. The link only percent-encodes
        // attribute-breaking characters, so it reads the same under `{{ }}` and
        // `{{{ }}}`, and a non-http(s) link drops the button.
        if let Some(Json::String(link)) = data.get("actionButtonLink").cloned() {
            if is_http_link(&link) {
                data.insert("actionButtonLink".into(), json!(attribute_safe_link(&link)));
            } else if !link.is_empty() {
                data.remove("actionButtonLink");
                data.remove("actionButtonText");
            }
        }
        data.insert(
            "unsubscribeAllUrl".into(),
            json!(sign(format!("/unsubscribe/{address}"))?),
        );
        data.insert(
            "unsubscribeOneUrl".into(),
            json!(sign(format!("/unsubscribe/{address}/{kind}"))?),
        );
        data.insert("accountLink".into(), json!(self.cfg.account_base_url));
        Ok(Some(Json::Object(data)))
    }

    /// One notification rendered for its type through the generic template;
    /// the id rides in `custom_args` so a redelivery is recognisable.
    pub async fn send_notification(
        &self,
        to: &str,
        address: &str,
        notification_id: Uuid,
        kind: &str,
        metadata: &Json,
    ) -> Result<(), SendFailure> {
        if !self.notifications_configured() {
            return Err(SendFailure::NotConfigured);
        }
        let Some(data) = self.notification_data(to, address, kind, metadata)? else {
            return Err(SendFailure::Permanent(format!(
                "no email template for {kind}"
            )));
        };
        let id = notification_id.to_string();
        let custom_args = json!({
            "tracking_id": id,
            "email_type": "notification",
            "notification_id": id,
        });
        self.send_generic(to, data, custom_args).await.map(|_| ())
    }
}

/// Percent-encodes what could leave a quoted `href` or start markup (quotes,
/// angle brackets, backtick, whitespace and controls); `&`, `=`, `?` stay raw.
fn attribute_safe_link(link: &str) -> String {
    let mut out = String::with_capacity(link.len());
    for c in link.chars() {
        if matches!(c, '"' | '\'' | '<' | '>' | '`') || c.is_whitespace() || c.is_control() {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// An absolute `http`/`https` URL.
fn is_http_link(link: &str) -> bool {
    url::Url::parse(link).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Turnstile's verdict and its `error-codes`. No secret configured passes
/// (the check is off); a missing token fails.
pub async fn verify_turnstile(
    http: &Outbound,
    secret: Option<&str>,
    token: Option<&str>,
) -> Result<(bool, Vec<String>), ApiError> {
    let Some(secret) = secret else {
        return Ok((true, Vec::new()));
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok((false, Vec::new()));
    };
    let form = format!(
        "secret={}&response={}",
        urlencoding::encode(secret),
        urlencoding::encode(token)
    );
    let resp = http
        .post(TURNSTILE_VERIFY_URL)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("turnstile request failed: {e}")))?;
    let body: Json = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("turnstile decode failed: {e}")))?;
    let codes = body["error-codes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok((body["success"].as_bool().unwrap_or(false), codes))
}

/// The refusal text of set-email and confirm-email.
pub const INVALID_EMAIL: &str = "Email address invalid. Please use a functioning email to continue";

/// `@dcl/schemas` `Email.validate`: `^[\w\-\.]+@([\w-]+\.)+[\w-]{2,}$` (ASCII `\w`).
pub fn is_valid_email(email: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    let labels: Vec<&str> = domain.split('.').collect();
    !local.is_empty()
        && local.chars().all(|c| word(c) || c == '.')
        && labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.chars().all(word))
        && labels.last().is_some_and(|l| l.len() >= 2)
}

/// Handlebars' `{{x}}` escaping, so rendered bodies match the old inbox's.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            '`' => out.push_str("&#x60;"),
            '=' => out.push_str("&#x3D;"),
            c => out.push(c),
        }
    }
    out
}

// from: notifications-workers@e2af2ee inbox/src/adapters/email-templates (Apache-2.0)
const VALIDATE_BUTTON: &str = r#"<p style='height: 41px; padding-top: 8px; padding-bottom: 8px;'>
  <a
    href='{link}'
    style='display: inline-block; background-color: #f03e53; color: #fff; text-decoration: none; padding: 10px 20px; border-radius: 5px; font-size: 16px; font-weight: bold;'
  >Click Here to Confirm Your Email</a>
</p>

<p>Or you can copy and paste this URL into the browser:
  <a href='{link}' style='font-size: 13px;'>{link}</a>
</p>"#;

/// The rendered validate-email / validate-credits-email template data.
pub fn validate_email_data(source: EmailSource, link: &str, user_name: Option<&str>) -> Json {
    let (intro, subject, banner, label) = match source {
        EmailSource::Account => (
            "<p>You're receiving this email because you signed up for email notifications from Decentraland.</p>\n<p>You'll be able to customize exactly what kind of notifications you'd like to receive (e.g., Marketplace, DAO, Events,\n  etc.), but you must first confirm your email.</p>",
            "[Action Needed] Confirm Your Email 📩",
            "dad9f8bb-b2c6-4d14-8a5c-27fcbbed0121",
            "Subscription Confirmation",
        ),
        EmailSource::Credits => (
            "<p>You're almost ready to start earning Marketplace Credits!<br>Click the button below to complete the email confirmation process.</p>",
            "[Action Needed] Confirm Your Email for Weekly Rewards 📩",
            "cb67605b-d594-4847-a163-34d0a1b27120",
            "Confirm Your Email",
        ),
    };
    let content = format!(
        "{intro}\n\n{}",
        VALIDATE_BUTTON.replace("{link}", &html_escape(link))
    );
    json!({
        "subject": subject,
        "content": content,
        "title": "Hey there",
        "titleHighlight": user_name.unwrap_or_default(),
        "bannerUrl": format!("http://cdn.mcauto-images-production.sendgrid.net/2322095804444b83/{banner}/600x240.png"),
        "bannerLabel": label,
    })
}

/// The rendered `common` template data of `POST /notifications/email`.
pub fn common_email_data(subject: &str, content: &str, title: &str, highlight: &str) -> Json {
    json!({
        "subject": html_escape(subject),
        "content": html_escape(content),
        "title": html_escape(title),
        "titleHighlight": highlight,
        "bannerUrl": "http://cdn.mcauto-images-production.sendgrid.net/2322095804444b83/519f099b-f5a6-4ef5-9d73-0319d560f6f5/600x240.png",
        "bannerLabel": "communication",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> EmailSender {
        EmailSender::new(
            EmailConfig {
                account_base_url: "https://account.decentraland.org".into(),
                ..Default::default()
            },
            Outbound::for_tests(),
        )
    }

    const ADDR: &str = "0x1234567890abcdef1234567890abcdef12345678";
    const CODE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef";

    fn dapps(flags: Json, variants: Json) -> DappsFlags {
        DappsFlags(Arc::new(Snapshot::from_document(
            json!({"flags": flags, "variants": variants}),
        )))
    }

    fn with_turnstile(secret: Option<&str>, flag: bool) -> EmailSender {
        EmailSender::new(
            EmailConfig {
                account_base_url: "https://account.decentraland.org".into(),
                turnstile_secret_key: secret.map(String::from),
                dapps_flags: dapps(json!({"dapps-turnstile-verification": flag}), json!({})),
                ..Default::default()
            },
            Outbound::for_tests(),
        )
    }

    #[test]
    fn turnstile_on_links_both_sources_to_the_challenge_page() {
        let s = with_turnstile(Some("secret"), true);
        assert_eq!(s.turnstile_secret(), Some("secret"));
        assert_eq!(
            s.confirm_url(EmailSource::Account, ADDR, CODE),
            format!(
                "https://account.decentraland.org/confirm-email-challenge/{CODE}?address={ADDR}&source=account"
            )
        );
        assert_eq!(
            s.confirm_url(EmailSource::Credits, ADDR, CODE),
            format!(
                "https://account.decentraland.org/confirm-email-challenge/{CODE}?address={ADDR}&source=credits"
            )
        );
    }

    #[test]
    fn turnstile_off_links_each_source_to_its_own_page() {
        for s in [
            with_turnstile(Some("secret"), false),
            with_turnstile(None, true),
            sender(),
        ] {
            assert_eq!(s.turnstile_secret(), None);
            assert_eq!(
                s.confirm_url(EmailSource::Account, ADDR, CODE),
                format!("https://account.decentraland.org/confirm-email/{CODE}?address={ADDR}")
            );
            assert_eq!(
                s.confirm_url(EmailSource::Credits, ADDR, CODE),
                format!(
                    "https://account.decentraland.org/credits-email-confirmed/{CODE}?address={ADDR}"
                )
            );
        }
    }

    #[test]
    fn the_send_url_honours_the_api_origin_and_an_empty_one_is_unconfigured() {
        assert_eq!(
            configured(false).send_url(),
            "https://api.sendgrid.com/v3/mail/send"
        );
        let mut cfg = configured(false).cfg;
        cfg.api_url = "https://sendgrid.example/".into();
        let s = EmailSender::new(cfg.clone(), Outbound::for_tests());
        assert_eq!(s.send_url(), "https://sendgrid.example/v3/mail/send");
        assert!(s.notifications_configured() && s.html_configured());
        cfg.api_url = String::new();
        let s = EmailSender::new(cfg, Outbound::for_tests());
        assert!(!s.notifications_configured() && !s.html_configured());
    }

    #[tokio::test]
    async fn an_empty_api_url_sends_nothing() {
        let mut cfg = configured(false).cfg;
        cfg.api_url = String::new();
        let s = EmailSender::new(cfg, Outbound::for_tests());
        assert_eq!(
            s.send_html("to@example.com", "s", "<p>h</p>", "h", &json!({}))
                .await,
            Err(SendFailure::NotConfigured)
        );
        assert_eq!(
            s.send_generic("to@example.com", json!({}), json!({})).await,
            Err(SendFailure::NotConfigured)
        );
    }

    #[test]
    fn a_missing_or_unloaded_flag_document_turns_both_flags_off() {
        for flags in [
            DappsFlags::default(),
            dapps(json!({}), json!({})),
            dapps(
                json!({"dapps-credits-blacklisted-domains": true}),
                json!({}),
            ),
        ] {
            let s = EmailSender::new(
                EmailConfig {
                    turnstile_secret_key: Some("secret".into()),
                    dapps_flags: flags,
                    ..Default::default()
                },
                Outbound::for_tests(),
            );
            assert_eq!(s.turnstile_secret(), None);
            assert!(s.turnstile_secret_without_flag());
            assert!(!s.is_domain_blacklisted("x@spam.com"));
        }
        assert!(!with_turnstile(Some("s"), true).turnstile_secret_without_flag());
        assert!(!with_turnstile(None, false).turnstile_secret_without_flag());
    }

    #[test]
    fn the_flag_variant_refuses_its_domains_while_the_flag_is_on() {
        let flagged = |on: bool| {
            EmailSender::new(
                EmailConfig {
                    dapps_flags: dapps(
                        json!({"dapps-credits-blacklisted-domains": on}),
                        json!({"dapps-credits-blacklisted-domains": {"payload": {"value": "Spam.com,\njunk.io "}}}),
                    ),
                    ..Default::default()
                },
                Outbound::for_tests(),
            )
        };
        let on = flagged(true);
        assert!(on.is_domain_blacklisted("x@SPAM.com"));
        assert!(on.is_domain_blacklisted("x@junk.io"));
        assert!(!on.is_domain_blacklisted("x@fine.org"));
        assert!(!flagged(false).is_domain_blacklisted("x@spam.com"));
    }

    async fn serve_list(status: u16, body: String) -> String {
        let app = axum::Router::new().route(
            "/list.conf",
            axum::routing::get(move || async move {
                (axum::http::StatusCode::from_u16(status).unwrap(), body)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}/list.conf")
    }

    #[tokio::test]
    async fn the_refresh_job_runs_only_with_a_list_url() {
        let jobs = Jobs::new();
        assert!(
            !sender().spawn_disposable_refresh(&jobs),
            "no URL (island, journey): no job, no fetch"
        );
        let list: String = (0..DISPOSABLE_MIN_DOMAINS)
            .map(|i| format!("d{i}.example\n"))
            .collect();
        let s = EmailSender::new(
            EmailConfig {
                disposable_list_url: serve_list(200, list).await,
                disposable_min_domains: DISPOSABLE_MIN_DOMAINS,
                ..Default::default()
            },
            Outbound::for_tests(),
        );
        assert!(s.spawn_disposable_refresh(&jobs));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !s.is_domain_blacklisted("x@d7.example") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fetched at start");
        jobs.shutdown(Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn disposable_domains_are_refused_and_a_failed_refresh_keeps_the_last_list() {
        let s = sender();
        assert!(
            !s.is_domain_blacklisted("x@mailinator.com"),
            "no list yet refuses nothing"
        );
        assert!(s.disposable_due());
        let ok = serve_list(200, "# comment\n\nMailinator.com\n  yopmail.com \n".into()).await;
        assert_eq!(s.refresh_disposable_domains(&ok, 2).await.unwrap(), 2);
        assert!(!s.disposable_due(), "fresh for an hour");
        assert!(s.is_domain_blacklisted("x@mailinator.com"));
        assert!(
            s.clone().is_domain_blacklisted("x@YOPMAIL.com"),
            "clones share the list"
        );
        assert!(!s.is_domain_blacklisted("x@fine.org"));
        let failing = serve_list(503, String::new()).await;
        assert!(s.refresh_disposable_domains(&failing, 2).await.is_err());
        let short = serve_list(200, "only.one\n".into()).await;
        assert!(
            s.refresh_disposable_domains(&short, 2).await.is_err(),
            "an implausibly small list is refused"
        );
        let huge: String = (0..DISPOSABLE_MAX_BYTES / 8)
            .map(|i| format!("{i:07}\n"))
            .collect::<String>()
            + "overflow.io\n";
        let err = s
            .refresh_disposable_domains(&serve_list(200, huge).await, 2)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("bytes"),
            "the download is capped: {err}"
        );
        assert!(
            s.is_domain_blacklisted("x@mailinator.com"),
            "kept on failure"
        );
    }

    #[test]
    fn email_validation_matches_dcl_schemas() {
        for ok in [
            "a@b.co",
            "first.last-x_1@sub.example.org",
            "a@b-c.io",
            ".@x.co",
        ] {
            assert!(is_valid_email(ok), "{ok}");
        }
        for bad in [
            "",
            "nope",
            "@x.com",
            "x@",
            "a+b@x.com",
            "a@x.c",
            "a@x",
            "a@.x.com",
            "a@x..com",
            "a@@x.com",
            "a b@x.com",
            "á@x.com",
            "a@x.com ",
        ] {
            assert!(!is_valid_email(bad), "{bad}");
        }
    }

    #[test]
    fn rendered_bodies_escape_like_handlebars() {
        let data = common_email_data("S&P", "<b>hi</b>", "New", "it's");
        assert_eq!(data["content"], "&lt;b&gt;hi&lt;/b&gt;");
        assert_eq!(data["subject"], "S&amp;P");
        assert_eq!(
            data["titleHighlight"], "it's",
            "raw: the template escapes it"
        );
        let link = with_turnstile(Some("s"), true).confirm_url(EmailSource::Credits, ADDR, CODE);
        let data = validate_email_data(EmailSource::Credits, &link, Some("Ann"));
        let content = data["content"].as_str().unwrap();
        assert!(content.starts_with("<p>You're almost ready"));
        assert!(content.contains(&format!("?address&#x3D;{ADDR}&amp;source&#x3D;credits'")));
        assert_eq!(data["titleHighlight"], "Ann");
        assert_eq!(
            validate_email_data(EmailSource::Account, &link, None)["titleHighlight"],
            ""
        );
    }

    #[test]
    fn base_url_trailing_slash_trimmed() {
        let s = EmailSender::new(
            EmailConfig {
                account_base_url: "https://account.decentraland.org/".into(),
                ..Default::default()
            },
            Outbound::for_tests(),
        );
        let url = s.confirm_url(EmailSource::Account, ADDR, CODE);
        assert!(url.starts_with("https://account.decentraland.org/confirm-email/"));
        assert!(!url.contains(".org//"));
    }

    #[tokio::test]
    async fn unconfigured_mailer_hard_fails() {
        let err = sender()
            .send_confirmation(EmailSource::Account, "user@example.com", ADDR, CODE, None)
            .await
            .expect_err("unconfigured mailer must error, never silently succeed");
        assert!(matches!(err, ApiError::Internal(_)));
        assert_eq!(
            sender()
                .send_notification(
                    "user@example.com",
                    ADDR,
                    Uuid::nil(),
                    "bid_received",
                    &json!({})
                )
                .await,
            Err(SendFailure::NotConfigured)
        );
        assert!(!sender().notifications_configured());
    }

    #[test]
    fn notification_data_matches_the_old_renderer_snapshot() {
        let s = EmailSender::new(
            EmailConfig {
                account_base_url: "https://decentraland.zone/account".into(),
                ..Default::default()
            },
            Outbound::for_tests(),
        );
        assert_eq!(
            s.notification_data("e@example.com", "0xa", "ban_lifted", &json!({})),
            Err(SendFailure::NoSigningKey),
            "no signing key, no unsubscribe link, no email"
        );
        let s = s.with_unsubscribe(
            Some("some-super-secret-key"),
            "https://notifications.decentraland.org/base-path-is-replaced/",
        );
        // notifications-workers processor/test/unit/adapters/__snapshots__/email-renderer.spec.ts.snap
        const OLD: &str = "0x1234567890ABCDEF1234567890ABCDEF12345678";
        let data = s
            .notification_data(
                "email@example.com",
                OLD,
                "ban_lifted",
                &json!({"userName": "TestUser"}),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            data,
            json!({
                "subject": "Your Access Has Been Restored",
                "address": "email@example.com",
                "content": "<p style=\"font-size: 16px; line-height: 24px; text-align: center; color: #43404A; padding: 0 24px;\">\n  Your ban has been lifted, and your account is active again. You can jump back into Decentraland.\n</p>\n",
                "actionButtonLink": "https://play.decentraland.org",
                "actionButtonText": "JUMP IN",
                "title": "",
                "titleHighlight": "TestUser",
                "bannerUrl": "http://cdn.mcauto-images-production.sendgrid.net/2322095804444b83/8da268fe-54f4-4b40-959e-afd3d44d1470/456x125.png",
                "bannerLabel": "Access Restored",
                "unsubscribeAllUrl": format!("https://notifications.decentraland.org/unsubscribe/{OLD}?signature=fd03dab02225f386464bdc0ea85ddffb0dd4ad1bc53e6be420224fd381cc58e5"),
                "unsubscribeOneUrl": format!("https://notifications.decentraland.org/unsubscribe/{OLD}/ban_lifted?signature=94a944ada0b1ac2d96533358f267ebbf75e5b437f89ef0abb20826346f175a15"),
                "accountLink": "https://decentraland.zone/account",
            })
        );
        assert_eq!(
            s.notification_data("e@example.com", "0xa", "land_transfer_received", &json!({})),
            Ok(None)
        );
    }

    fn configured(sandbox_mode: bool) -> EmailSender {
        EmailSender::new(
            EmailConfig {
                sendgrid_api_key: Some("sg-example".into()),
                from_email: Some("noreply@example.com".into()),
                notification_template_id: Some("d-generic".into()),
                api_url: "https://api.sendgrid.com".into(),
                account_base_url: "https://account.decentraland.org".into(),
                sandbox_mode,
                environment: "prd".into(),
                ..Default::default()
            },
            Outbound::for_tests(),
        )
        .with_unsubscribe(Some("k"), "https://notifications.decentraland.org")
    }

    #[test]
    fn the_template_body_carries_the_sender_name_custom_args_and_sandbox_flag() {
        for sandbox in [false, true] {
            let body = configured(sandbox).template_body(
                "noreply@example.com",
                "d-generic",
                "to@example.com",
                json!({"subject": "s"}),
                json!({"tracking_id": "t1", "email_type": "notification", "notification_id": "t1"}),
            );
            assert_eq!(
                body["from"],
                json!({"email": "noreply@example.com", "name": "Decentraland"})
            );
            assert_eq!(body["template_id"], "d-generic");
            assert_eq!(
                body["custom_args"],
                json!({"tracking_id": "t1", "email_type": "notification", "notification_id": "t1", "environment": "prd"})
            );
            assert_eq!(
                body["personalizations"][0],
                json!({"to": [{"email": "to@example.com"}], "dynamic_template_data": {"subject": "s", "address": "to@example.com"}})
            );
            assert_eq!(body["mail_settings"]["sandbox_mode"]["enable"], sandbox);
        }
        let body = configured(false).template_body(
            "f@example.com",
            "d",
            "t@example.com",
            json!({}),
            json!({}),
        );
        assert!(
            body["custom_args"]["tracking_id"]
                .as_str()
                .is_some_and(|t| t.len() == 36),
            "a fresh tracking id when none is given"
        );
    }

    #[test]
    fn notification_data_passes_the_name_raw_and_keeps_only_http_links() {
        let s = configured(false);
        let data = |metadata: Json| {
            s.notification_data("e@example.com", "0xa", "item_sold", &metadata)
                .unwrap()
                .unwrap()
        };
        let d = data(json!({"userName": "<b>x</b>", "link": "https://market.example/a?b=1&c=2"}));
        assert_eq!(d["titleHighlight"], "<b>x</b>", "upstream sends it raw");
        assert_eq!(
            d["actionButtonLink"], "https://market.example/a?b=1&c=2",
            "a query string survives unchanged (no HTML escaping to double)"
        );
        let d = data(json!({"userName": "u", "link": "https://x.example/a?q=\"><script>`'\n b"}));
        let link = d["actionButtonLink"].as_str().unwrap();
        assert_eq!(
            link,
            "https://x.example/a?q=%22%3E%3Cscript%3E%60%27%0A%20b"
        );
        assert!(!link.contains(['"', '\'', '<', '>', '`', ' ']), "{link}");
        for bad in ["javascript:alert(1)", "data:text/html,x", "/relative"] {
            let d = data(json!({"userName": "u", "link": bad}));
            assert!(d.get("actionButtonLink").is_none(), "{bad}");
            assert!(d.get("actionButtonText").is_none(), "{bad}");
        }
    }

    #[test]
    fn confirmation_uses_its_own_template_when_set_else_the_generic_one() {
        let generic = configured(false);
        assert_eq!(
            generic.cfg.notification_template_id.as_deref(),
            Some("d-generic")
        );
        let mut cfg = generic.cfg.clone();
        cfg.validate_credits_email_template_id = Some("d-credits".into());
        let own = EmailSender::new(cfg, Outbound::for_tests());
        assert_eq!(
            own.confirmation_template(EmailSource::Credits),
            (Some("d-credits"), true)
        );
        assert_eq!(
            own.confirmation_template(EmailSource::Account),
            (Some("d-generic"), false)
        );
    }

    #[test]
    fn code_is_32_chars_and_unique() {
        let a = make_code();
        assert_eq!(a.len(), CODE_LEN);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, make_code());
    }

    #[test]
    fn domain_blacklist_parses_and_matches_case_insensitively() {
        let s = EmailSender::new(
            EmailConfig {
                domain_blacklist: EmailConfig::parse_domain_blacklist(" Spam.com, ,junk.io"),
                ..Default::default()
            },
            Outbound::for_tests(),
        );
        assert!(s.is_domain_blacklisted("x@SPAM.com"));
        assert!(s.is_domain_blacklisted("x@junk.io"));
        assert!(!s.is_domain_blacklisted("x@fine.org"));
        assert!(!s.is_domain_blacklisted("first.last@fine.org"));
        for bad in ["no-at-sign", "a.b.c@fine.org", ".a@fine.org", "a.@fine.org"] {
            assert!(s.is_domain_blacklisted(bad), "{bad}");
        }
        assert!(
            !sender().is_domain_blacklisted("x@spam.com"),
            "an empty list refuses no domain"
        );
    }

    #[tokio::test]
    async fn turnstile_without_a_secret_passes_and_without_a_token_fails() {
        let http = Outbound::for_tests();
        assert!(verify_turnstile(&http, None, None).await.unwrap().0);
        assert!(!verify_turnstile(&http, Some("s"), None).await.unwrap().0);
        assert!(
            !verify_turnstile(&http, Some("s"), Some(""))
                .await
                .unwrap()
                .0
        );
    }
}
