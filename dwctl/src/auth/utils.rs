//! Authentication utility functions.

/// Derive a default display name from an email address.
///
/// Uses the local part (the text before `@`), falling back to `"user"` when
/// the email has no `@` or the local part is empty. This replaces the previous
/// randomly-generated "{adjective} {noun} {number}" placeholder so a new
/// user's name is recognisable — derived from their email, not invented.
///
/// Examples:
///   `seb@doubleword.ai`      → `seb`
///   `user.name@domain.co.uk` → `user.name`
///   `@domain.com`            → `user`
///   `no-at-sign`             → `no-at-sign`
pub fn default_display_name(email: &str) -> String {
    let prefix = email.rsplit_once('@').map_or(email, |(local, _)| local);
    if prefix.is_empty() {
        "user".to_string()
    } else {
        prefix.to_string()
    }
}

/// Extract the domain part from an email address.
/// Returns `None` if the email doesn't contain an `@`.
/// The domain part of an email address, lowercased.
///
/// Normalised because domains are case-insensitive but the things we compare
/// them against are not. Without it `Alice@Acme.com` claims `Acme.com`, a
/// later `bob@acme.com` fails to match it and gets no join request, and -
/// worse - `x@GMAIL.com` slips past `is_personal_email_domain` and claims
/// gmail.com as a company domain.
pub fn email_domain(email: &str) -> Option<String> {
    email.rsplit_once('@').map(|(_, domain)| domain.to_ascii_lowercase())
}

/// Returns `true` if the domain belongs to a personal/free email provider
/// where auto-org creation would be inappropriate.
pub fn is_personal_email_domain(domain: &str) -> bool {
    const PERSONAL_DOMAINS: &[&str] = &[
        // Major providers
        "gmail.com",
        "googlemail.com",
        "hotmail.com",
        "hotmail.co.uk",
        "live.com",
        "live.fr",
        "outlook.com",
        "msn.com",
        "yahoo.com",
        "yahoo.co.uk",
        "yahoo.co.jp",
        "ymail.com",
        "aol.com",
        "aim.com",
        "icloud.com",
        "me.com",
        "mac.com",
        "mail.com",
        "zoho.com",
        "yandex.com",
        "163.com",
        // Privacy-focused
        "protonmail.com",
        "protonmail.ch",
        "proton.me",
        "tutanota.com",
        "tuta.com",
        "fastmail.com",
        // Regional/misc
        "gmx.com",
        "gmx.de",
        "gmx.net",
        // Privacy relays and aliases
        "privaterelay.appleid.com",
        "mozmail.com",
        "duck.com",
        "passmail.net",
    ];

    let lower = domain.to_lowercase();
    PERSONAL_DOMAINS.contains(&lower.as_str())
}
