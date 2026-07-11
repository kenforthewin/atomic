//! Email delivery for magic links, behind a trait.
//!
//! Magic-link-only auth makes email delivery the critical path (plan: open
//! question "Email deliverability"), so the abstraction is deliberately
//! narrow — one method, one message kind — and the dangerous part is a
//! policy, not a convention:
//!
//! - **The link is the credential.** Only [`LogSender`] (dev mode, where the
//!   log *is* the delivery channel) may ever write it to logs.
//!   [`MailgunSender`] never logs links, and its error values carry provider
//!   status/body text, never the link.
//! - **Tests never send real email.** The integration suites implement
//!   [`EmailSender`] with a capturing sender (`tests/support`) and assert on
//!   the captured messages, including extracting the exact link.
//!
//! The `serve` binary selects the implementation via `--email-mode
//! mailgun|log`; see `main.rs`.

use async_trait::async_trait;

use crate::error::CloudError;
use crate::magic_links::MagicLinkPurpose;

/// Delivers a magic-link message to one recipient. `link` is the full URL
/// the recipient must open (`https://<app-host>/signup/complete?token=…`);
/// `purpose` selects the copy (finish signup vs. sign in).
#[async_trait]
pub trait EmailSender: Send + Sync {
    async fn send_magic_link(
        &self,
        to: &str,
        link: &str,
        purpose: MagicLinkPurpose,
    ) -> Result<(), CloudError>;

    /// Deliver a billing-lifecycle notice (trial ending / trial ended).
    /// Unlike magic links these carry no credential — the button URL is the
    /// public billing page, session-gated on arrival — so implementations
    /// may log them freely. The no-real-email-in-tests policy still applies.
    async fn send_billing_notice(&self, to: &str, notice: &BillingNotice)
        -> Result<(), CloudError>;
}

/// One billing-lifecycle email, fully composed. The copy constructors below
/// are the only places trial copy lives (decision 2026-07-11: plan-expiry
/// communication is email-only — the shared product frontend gets no
/// cloud-specific expiry UI, so these messages and the `/account/*`
/// dashboard are the entire comms surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingNotice {
    pub subject: String,
    /// Lead paragraph, shown above the button.
    pub lead: String,
    /// Optional second paragraph (what happens next / what Free means).
    pub detail: Option<String>,
    pub button_label: &'static str,
    /// Where the button lands: the account dashboard's billing page on the
    /// recipient's tenant host.
    pub button_url: String,
}

impl BillingNotice {
    /// "Your trial ends soon" — sent once, a few days ahead (the
    /// `trial_warning_sent_at` claim in the sweep guarantees the once).
    pub fn trial_ending(days_left: i64, billing_url: String) -> Self {
        let when = match days_left {
            ..=1 => "tomorrow".to_string(),
            n => format!("in {n} days"),
        };
        Self {
            subject: format!("Your Atomic Pro trial ends {when}"),
            lead: format!(
                "Your free trial of Atomic Pro ends {when}. To keep unlimited \
                 atoms, unlimited knowledge bases, and frontier AI models, \
                 upgrade any time from your billing page."
            ),
            detail: Some(
                "If you do nothing, your account simply moves to the Free plan — \
                 your data is never deleted."
                    .to_string(),
            ),
            button_label: "Upgrade to Pro",
            button_url: billing_url,
        }
    }

    /// "Your trial has ended" — sent by the downgrade sweep the moment the
    /// account drops to Free. `read_only` selects the over-limit variant
    /// (writes blocked until the account trims below the Free limits or
    /// upgrades; data always retained).
    pub fn trial_ended(read_only: bool, billing_url: String) -> Self {
        let detail = if read_only {
            "Your knowledge base holds more than the Free plan covers, so your \
             account is read-only for now — everything remains available to read \
             and export, and nothing is ever deleted. Upgrade to Pro to pick up \
             writing right where you left off."
        } else {
            "The Free plan covers the essentials. Upgrade to Pro any time for \
             unlimited atoms, unlimited knowledge bases, and frontier AI models."
        };
        Self {
            subject: "Your Atomic Pro trial has ended".to_string(),
            lead: "Your free trial of Atomic Pro has ended and your account is \
                   now on the Free plan. All of your data is untouched."
                .to_string(),
            detail: Some(detail.to_string()),
            button_label: "Upgrade to Pro",
            button_url: billing_url,
        }
    }
}

/// Dev-mode sender: writes the link to the server log at info level instead
/// of sending anything. The log line is the delivery channel — copy the URL
/// from the console. Never select this where logs are aggregated or shared;
/// `--email-mode mailgun` exists for that, and it never logs links.
pub struct LogSender;

#[async_trait]
impl EmailSender for LogSender {
    async fn send_magic_link(
        &self,
        to: &str,
        link: &str,
        purpose: MagicLinkPurpose,
    ) -> Result<(), CloudError> {
        tracing::info!(
            to,
            purpose = purpose.as_str(),
            link,
            "magic link issued (log email mode — link printed instead of emailed)"
        );
        Ok(())
    }

    async fn send_billing_notice(
        &self,
        to: &str,
        notice: &BillingNotice,
    ) -> Result<(), CloudError> {
        tracing::info!(
            to,
            subject = %notice.subject,
            url = %notice.button_url,
            "billing notice (log email mode — printed instead of emailed)"
        );
        Ok(())
    }
}

/// Sends through the Mailgun REST API (`POST /v3/<domain>/messages`).
/// Salvaged from the pre-rewrite prototype's client and adapted to the
/// trait; the message copy varies by purpose. Errors carry Mailgun's status
/// and response body — never the link.
pub struct MailgunSender {
    api_key: String,
    domain: String,
    from: String,
    http: reqwest::Client,
}

impl MailgunSender {
    /// `domain` is the Mailgun sending domain (`mg.atomic.cloud`); `from`
    /// the RFC 5322 sender (`Atomic <no-reply@mg.atomic.cloud>`).
    pub fn new(api_key: String, domain: String, from: String) -> Self {
        Self {
            api_key,
            domain,
            from,
            http: reqwest::Client::new(),
        }
    }

    /// POST one composed message to Mailgun. Error values carry provider
    /// status/body only — callers must never put links or tokens in
    /// `subject`/`text`/`html` they wouldn't want in an error log (the
    /// magic-link path relies on this).
    async fn deliver(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), CloudError> {
        let url = format!("https://api.mailgun.net/v3/{}/messages", self.domain);
        let resp = self
            .http
            .post(&url)
            .basic_auth("api", Some(&self.api_key))
            .form(&[
                ("from", self.from.as_str()),
                ("to", to),
                ("subject", subject),
                ("text", text),
                ("html", html),
            ])
            .send()
            .await
            .map_err(|e| CloudError::EmailSend(format!("Mailgun request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CloudError::EmailSend(format!(
                "Mailgun returned {status}: {body}"
            )));
        }
        Ok(())
    }
}

/// The shared HTML shell: heading, lead paragraph(s), one button, footer.
/// Both message kinds render through this so the brand look stays in one
/// place.
fn html_shell(subject: &str, lead: &str, detail: Option<&str>, button: &str, url: &str) -> String {
    let detail_html = detail
        .map(|d| {
            format!(
                r#"<p style="color: #4a4540; line-height: 1.6; margin-bottom: 32px;">{d}</p>"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"<div style="font-family: -apple-system, system-ui, sans-serif; max-width: 480px; margin: 0 auto; padding: 40px 20px;">
  <h1 style="font-size: 24px; font-weight: normal; margin-bottom: 24px;">{subject}</h1>
  <p style="color: #4a4540; line-height: 1.6; margin-bottom: 32px;">{lead}</p>
  {detail_html}
  <a href="{url}" style="display: inline-block; background: #7c3aed; color: white; text-decoration: none; padding: 12px 32px; border-radius: 8px; font-weight: 500;">{button}</a>
  <p style="color: #b3aea8; font-size: 12px; margin-top: 24px; border-top: 1px solid #eee7df; padding-top: 16px;">Atomic · <a href="https://atomicapp.ai" style="color: #8a8580;">atomicapp.ai</a> · <a href="https://discord.gg/fT4vTERhz3" style="color: #8a8580;">Community &amp; support</a></p>
</div>"#
    )
}

/// Subject line and lead-in sentence per purpose.
fn copy_for(purpose: MagicLinkPurpose) -> (&'static str, &'static str, &'static str) {
    match purpose {
        MagicLinkPurpose::Signup => (
            "Finish creating your Atomic account",
            "Click the button below to finish creating your account.",
            "Create account",
        ),
        MagicLinkPurpose::Login => (
            "Sign in to Atomic",
            "Click the button below to sign in to your account.",
            "Sign in",
        ),
    }
}

#[async_trait]
impl EmailSender for MailgunSender {
    async fn send_magic_link(
        &self,
        to: &str,
        link: &str,
        purpose: MagicLinkPurpose,
    ) -> Result<(), CloudError> {
        let (subject, lead, button) = copy_for(purpose);
        let text = format!(
            "{lead}\n\n{link}\n\nThis link expires in 15 minutes. \
             If you didn't request it, you can ignore this email.\n\n\
             — Atomic · https://atomicapp.ai · Community & support: https://discord.gg/fT4vTERhz3"
        );
        let html = format!(
            r#"<div style="font-family: -apple-system, system-ui, sans-serif; max-width: 480px; margin: 0 auto; padding: 40px 20px;">
  <h1 style="font-size: 24px; font-weight: normal; margin-bottom: 24px;">{subject}</h1>
  <p style="color: #4a4540; line-height: 1.6; margin-bottom: 32px;">{lead} This link expires in 15 minutes.</p>
  <a href="{link}" style="display: inline-block; background: #7c3aed; color: white; text-decoration: none; padding: 12px 32px; border-radius: 8px; font-weight: 500;">{button}</a>
  <p style="color: #8a8580; font-size: 13px; margin-top: 32px;">If you didn't request this, you can ignore this email.</p>
  <p style="color: #b3aea8; font-size: 12px; margin-top: 24px; border-top: 1px solid #eee7df; padding-top: 16px;">Atomic · <a href="https://atomicapp.ai" style="color: #8a8580;">atomicapp.ai</a> · <a href="https://discord.gg/fT4vTERhz3" style="color: #8a8580;">Community &amp; support</a></p>
</div>"#
        );

        self.deliver(to, subject, &text, &html).await
    }

    async fn send_billing_notice(
        &self,
        to: &str,
        notice: &BillingNotice,
    ) -> Result<(), CloudError> {
        let detail_text = notice
            .detail
            .as_deref()
            .map(|d| format!("{d}\n\n"))
            .unwrap_or_default();
        let text = format!(
            "{}\n\n{}{}: {}\n\n\
             — Atomic · https://atomicapp.ai · Community & support: https://discord.gg/fT4vTERhz3",
            notice.lead, detail_text, notice.button_label, notice.button_url
        );
        let html = html_shell(
            &notice.subject,
            &notice.lead,
            notice.detail.as_deref(),
            notice.button_label,
            &notice.button_url,
        );
        self.deliver(to, &notice.subject, &text, &html).await
    }
}
