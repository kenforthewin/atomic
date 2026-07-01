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

        let url = format!("https://api.mailgun.net/v3/{}/messages", self.domain);
        let resp = self
            .http
            .post(&url)
            .basic_auth("api", Some(&self.api_key))
            .form(&[
                ("from", self.from.as_str()),
                ("to", to),
                ("subject", subject),
                ("text", text.as_str()),
                ("html", html.as_str()),
            ])
            .send()
            .await
            .map_err(|e| CloudError::EmailSend(format!("Mailgun request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            // Provider status + body only — the link must never ride along
            // into error values or logs.
            let body = resp.text().await.unwrap_or_default();
            return Err(CloudError::EmailSend(format!(
                "Mailgun returned {status}: {body}"
            )));
        }
        Ok(())
    }
}
