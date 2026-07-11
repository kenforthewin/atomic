//! Trial lifecycle emails — the entire proactive comms surface for plan
//! expiry (decision 2026-07-11: **email-only**; the shared product frontend
//! gets no cloud-specific expiry popups, and the only in-product billing UI
//! is the cloud dashboard at `/account/*`).
//!
//! Two messages, both composed in [`BillingNotice`](crate::email::BillingNotice)
//! and delivered through the same [`EmailSender`] as magic links:
//!
//! - **"Trial ends soon"** — sent once, [`DEFAULT_TRIAL_WARNING_LEAD_DAYS`]
//!   ahead. Exactly-once is the claim's job: [`claim_trial_warnings`] sets
//!   `accounts.trial_warning_sent_at` in the same UPDATE that selects, so a
//!   concurrent sweep on another pod claims disjoint rows, and a failed send
//!   re-arms its row ([`reset_trial_warning`]) for the next hourly sweep.
//! - **"Trial ended"** — sent for each account the expiry sweep actually
//!   downgraded ([`TrialAdvance::downgraded`]). The downgrade UPDATE
//!   (guarded on `billing_state = 'trialing'`) fires once per trial, so
//!   this needs no marker; the send is best-effort — a failure is logged
//!   and dropped rather than retried, because the state change it reports
//!   already happened and the dashboard shows the same fact.
//!
//! Both link to the recipient's own dashboard billing page
//! (`https://<subdomain>.<base>/account/billing`), which is session-gated
//! on arrival (the login redirect brings them back).

use chrono::{DateTime, Utc};

use crate::billing::dunning::{
    claim_trial_warnings, reset_trial_warning, TrialDowngraded, DEFAULT_TRIAL_WARNING_LEAD_DAYS,
};
use crate::control_plane::ControlPlane;
use crate::email::{BillingNotice, EmailSender};
use crate::error::CloudError;

/// The dashboard billing page on the account's tenant host.
fn billing_url(subdomain: &str, base_domain: &str) -> String {
    format!("https://{subdomain}.{base_domain}/account/billing")
}

/// Whole days until `ends_at`, rounded up (an email claiming "ends in 0
/// days" reads broken; within the last 24h this yields 1 → "tomorrow").
fn days_until(now: DateTime<Utc>, ends_at: DateTime<Utc>) -> i64 {
    let secs = (ends_at - now).num_seconds().max(0);
    (secs + 86_399) / 86_400
}

/// One "trial ends soon" pass: claim every due account and send its warning.
/// Returns how many were sent; a failed send logs, re-arms the claim, and
/// does not abort the rest of the pass.
pub async fn send_trial_warnings(
    control: &ControlPlane,
    sender: &dyn EmailSender,
    base_domain: &str,
    now: DateTime<Utc>,
) -> Result<u64, CloudError> {
    let claimed = claim_trial_warnings(control, now, DEFAULT_TRIAL_WARNING_LEAD_DAYS).await?;
    let mut sent = 0;
    for warning in claimed {
        let notice = BillingNotice::trial_ending(
            days_until(now, warning.trial_ends_at),
            billing_url(&warning.subdomain, base_domain),
        );
        match sender.send_billing_notice(&warning.email, &notice).await {
            Ok(()) => sent += 1,
            Err(e) => {
                tracing::error!(
                    account_id = %warning.account_id,
                    error = %e,
                    "trial warning email failed; re-arming for the next sweep"
                );
                reset_trial_warning(control, &warning.account_id).await?;
            }
        }
    }
    if sent > 0 {
        tracing::info!(sent, "trial warning emails sent");
    }
    Ok(sent)
}

/// Send the "trial ended" notice for each account a
/// [`advance_expired_trials`](crate::billing::dunning::advance_expired_trials)
/// pass downgraded. Best-effort per recipient (see module docs); returns how
/// many sends succeeded.
pub async fn notify_trial_downgrades(
    sender: &dyn EmailSender,
    base_domain: &str,
    downgraded: &[TrialDowngraded],
) -> u64 {
    let mut sent = 0;
    for account in downgraded {
        let notice = BillingNotice::trial_ended(
            account.read_only,
            billing_url(&account.subdomain, base_domain),
        );
        match sender.send_billing_notice(&account.email, &notice).await {
            Ok(()) => sent += 1,
            Err(e) => tracing::error!(
                account_id = %account.account_id,
                error = %e,
                "trial-ended email failed; not retried (the dashboard shows the same fact)"
            ),
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_until_rounds_up_and_floors_at_zero() {
        let now = chrono::Utc::now();
        assert_eq!(days_until(now, now + chrono::Duration::hours(1)), 1);
        assert_eq!(days_until(now, now + chrono::Duration::hours(25)), 2);
        assert_eq!(days_until(now, now + chrono::Duration::days(3)), 3);
        assert_eq!(days_until(now, now - chrono::Duration::hours(5)), 0);
    }

    #[test]
    fn notice_copy_matches_the_moment() {
        let ending = BillingNotice::trial_ending(1, "https://a.example/account/billing".into());
        assert!(ending.subject.contains("tomorrow"));
        let ending3 = BillingNotice::trial_ending(3, "https://a.example/account/billing".into());
        assert!(ending3.subject.contains("in 3 days"));

        let ended_ro = BillingNotice::trial_ended(true, "https://a.example/account/billing".into());
        assert!(ended_ro.detail.as_deref().unwrap().contains("read-only"));
        let ended_ok =
            BillingNotice::trial_ended(false, "https://a.example/account/billing".into());
        assert!(!ended_ok.detail.as_deref().unwrap().contains("read-only"));
    }
}
