//! Static blocklist of subdomains accounts may never claim.
//!
//! Two distinct mechanisms guard the subdomain namespace:
//!
//! 1. **This list** — names the platform itself uses or that would confuse
//!    users if tenant-owned (`www`, `api`, `mail`, ...). Compiled in, never
//!    expires.
//! 2. **The `subdomains_reserved` control-plane table** — time-boxed holds
//!    on subdomains freed by account deletion (90 days, per the plan's
//!    deletion sequence), so stale external clients don't silently reach a
//!    stranger's account.
//!
//! Signup validation checks both.

/// Reserved subdomains, ASCII-sorted so [`is_reserved`] can binary-search.
/// The unit test below pins the ordering invariant.
static RESERVED: &[&str] = &[
    "about",
    "abuse",
    "account",
    "accounts",
    "admin",
    "api",
    "app",
    "assets",
    "auth",
    "beta",
    "billing",
    "blog",
    "cdn",
    "chat",
    "cloud",
    "community",
    "console",
    "contact",
    "dashboard",
    "demo",
    "dev",
    "developer",
    "developers",
    "dns",
    "docs",
    "download",
    "downloads",
    "email",
    "files",
    "forum",
    "ftp",
    "git",
    "help",
    "home",
    "imap",
    "internal",
    "legal",
    "login",
    "logout",
    "mail",
    "marketing",
    "mcp",
    "media",
    "metrics",
    "monitoring",
    "news",
    "ns1",
    "ns2",
    "oauth",
    "portal",
    "postmaster",
    "pricing",
    "privacy",
    "prod",
    "production",
    "proxy",
    "register",
    "root",
    "sales",
    "secure",
    "security",
    "settings",
    "signin",
    "signup",
    "smtp",
    "staging",
    "static",
    "stats",
    "status",
    "store",
    "support",
    "system",
    "team",
    "terms",
    "test",
    "testing",
    "vpn",
    "web",
    "webmail",
    "wiki",
    "www",
];

/// Whether `subdomain` is on the static platform blocklist.
///
/// Matching is case-insensitive even though the signup slug rule
/// (`[a-z0-9-]{3,32}`) only admits lowercase — defense in depth against a
/// validation-order mistake upstream.
pub fn is_reserved(subdomain: &str) -> bool {
    let needle = subdomain.to_ascii_lowercase();
    RESERVED.binary_search(&needle.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binary search requires strict ascending order; a careless insertion
    /// would silently break lookups for everything after it.
    #[test]
    fn reserved_list_is_sorted_and_deduped() {
        for pair in RESERVED.windows(2) {
            assert!(
                pair[0] < pair[1],
                "RESERVED out of order or duplicated: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }
}
