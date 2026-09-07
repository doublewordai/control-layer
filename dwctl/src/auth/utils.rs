//! Authentication utility functions.

/// Derive a default display name from an email address.
///
/// Uses the local part (the text before `@`). If there's no `@`, returns the
/// input string unchanged. Falls back to `"user"` when the local part is empty.
/// This replaces the previous randomly-generated "{adjective} {noun} {number}"
/// placeholder so a new user's name is recognisable — derived from their email, not invented.
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
    let (_, domain) = email.rsplit_once('@')?;
    // A trailing dot is the DNS root and makes "acme.com." a different string
    // to "acme.com", which would claim the same company twice. `None` rather
    // than an empty string for "bob@": an empty domain matches nothing worth
    // matching, and letting it reach `find_by_domain` turns its
    // `$1 || '~%'` arm into a bare `~%` wildcard.
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() { None } else { Some(domain) }
}

/// Returns `true` if the domain belongs to a personal/free email provider
/// where auto-org creation would be inappropriate.
///
/// This is the built-in baseline. Deployments extend it through
/// `auth.personal_email_domains`; call sites should go through
/// [`crate::config::AuthConfig::is_personal_email_domain`] rather than calling
/// this directly, so operators can close a gap without waiting for a release.
///
/// A domain being on this list makes it unclaimable *and* unroutable: the
/// three `find_by_domain` call sites all filter personal domains out first, so
/// adding an entry here retires any existing claim on it as well as preventing
/// new ones.
///
/// Being absent is the dangerous direction. A consumer provider that is missing
/// gets a perfectly legitimate `{domain}~{suffix}` claim from whoever signs up
/// first, and `ORDER BY created_at ASC LIMIT 1` makes that first claimant
/// permanent for everyone who follows - which is how one workspace came to hold
/// 66 unrelated people who all happened to use the same free mail provider. Err
/// towards including a domain: the cost of a false positive is that one company
/// on an unusual domain does not get automatic colleague-matching, and the cost
/// of a false negative is that provider's entire user base in one stranger's
/// workspace.
pub fn is_builtin_personal_email_domain(domain: &str) -> bool {
    const PERSONAL_DOMAINS: &[&str] = &[
        // Major providers
        "gmail.com",
        "googlemail.com",
        "hotmail.com",
        "hotmail.co.uk",
        "hotmail.fr",
        "hotmail.de",
        "hotmail.es",
        "hotmail.it",
        "hotmail.be",
        "hotmail.nl",
        "hotmail.se",
        "hotmail.ca",
        "hotmail.com.au",
        "hotmail.com.br",
        "live.com",
        "live.fr",
        "live.co.uk",
        "live.de",
        "live.it",
        "live.nl",
        "live.se",
        "live.ca",
        "live.com.au",
        "live.com.mx",
        "outlook.com",
        "outlook.fr",
        "outlook.de",
        "outlook.es",
        "outlook.it",
        "outlook.be",
        "outlook.dk",
        "outlook.ie",
        "outlook.jp",
        "outlook.co.id",
        "outlook.com.au",
        "outlook.com.br",
        "msn.com",
        "msn.co.uk",
        "passport.com",
        "windowslive.com",
        "yahoo.com",
        "yahoo.co.uk",
        "yahoo.co.jp",
        "yahoo.co.in",
        "yahoo.co.id",
        "yahoo.co.kr",
        "yahoo.co.nz",
        "yahoo.com.ar",
        "yahoo.com.au",
        "yahoo.com.br",
        "yahoo.com.hk",
        "yahoo.com.mx",
        "yahoo.com.ph",
        "yahoo.com.sg",
        "yahoo.com.tw",
        "yahoo.com.vn",
        "yahoo.de",
        "yahoo.dk",
        "yahoo.es",
        "yahoo.fr",
        "yahoo.gr",
        "yahoo.ie",
        "yahoo.in",
        "yahoo.it",
        "yahoo.nl",
        "yahoo.no",
        "yahoo.pl",
        "yahoo.pt",
        "yahoo.ro",
        "yahoo.se",
        "ymail.com",
        "rocketmail.com",
        "aol.com",
        "aol.co.uk",
        "aol.de",
        "aol.fr",
        "aim.com",
        "icloud.com",
        "me.com",
        "mac.com",
        "mail.com",
        "email.com",
        "usa.com",
        "zoho.com",
        "zohomail.com",
        // Yandex and the Russian/Ukrainian free providers. `yandex.com` alone
        // let `yandex.ru` be claimed in production.
        "yandex.com",
        "yandex.ru",
        "yandex.by",
        "yandex.kz",
        "yandex.ua",
        "ya.ru",
        "mail.ru",
        "bk.ru",
        "inbox.ru",
        "list.ru",
        "internet.ru",
        "rambler.ru",
        "ukr.net",
        "meta.ua",
        // Chinese providers. NetEase runs both 163 and 126; Tencent's qq.com
        // was claimed in production and had swept up 65 unrelated signups.
        "qq.com",
        "foxmail.com",
        "163.com",
        "126.com",
        "yeah.net",
        "sina.com",
        "sina.cn",
        "sohu.com",
        "aliyun.com",
        "tom.com",
        "21cn.com",
        // Other Asian consumer providers
        "naver.com",
        "hanmail.net",
        "daum.net",
        "nate.com",
        "docomo.ne.jp",
        "ezweb.ne.jp",
        "softbank.ne.jp",
        "nifty.com",
        "biglobe.ne.jp",
        "rediffmail.com",
        "sify.com",
        // Privacy-focused
        "protonmail.com",
        "protonmail.ch",
        "proton.me",
        "pm.me",
        "tutanota.com",
        "tutanota.de",
        "tutamail.com",
        "tuta.com",
        "tuta.io",
        "fastmail.com",
        "fastmail.fm",
        "hushmail.com",
        "posteo.de",
        "mailbox.org",
        "runbox.com",
        "startmail.com",
        "disroot.org",
        "riseup.net",
        "zoho.eu",
        // European free providers
        "gmx.com",
        "gmx.de",
        "gmx.net",
        "gmx.at",
        "gmx.ch",
        "gmx.fr",
        "gmx.co.uk",
        "gmx.es",
        "web.de",
        "t-online.de",
        "freenet.de",
        "arcor.de",
        "online.de",
        "email.de",
        "libero.it",
        "virgilio.it",
        "alice.it",
        "tiscali.it",
        "tin.it",
        "fastwebnet.it",
        "wanadoo.fr",
        "orange.fr",
        "free.fr",
        "sfr.fr",
        "laposte.net",
        "neuf.fr",
        "bbox.fr",
        "club-internet.fr",
        "voila.fr",
        "terra.es",
        "telefonica.net",
        "wp.pl",
        "o2.pl",
        "onet.pl",
        "interia.pl",
        "gazeta.pl",
        "seznam.cz",
        "centrum.cz",
        "post.cz",
        "abv.bg",
        "mail.bg",
        "freemail.hu",
        "citromail.hu",
        "sapo.pt",
        "telenet.be",
        "skynet.be",
        "ziggo.nl",
        "kpnmail.nl",
        "home.nl",
        "telia.com",
        "bredband.net",
        "online.no",
        "bluewin.ch",
        "hispeed.ch",
        "mynet.com",
        "hotmail.com.tr",
        // Consumer ISPs. These read like company domains but belong to
        // everybody on that ISP; `comcast.net` was claimed in production.
        "comcast.net",
        "verizon.net",
        "att.net",
        "sbcglobal.net",
        "bellsouth.net",
        "cox.net",
        "charter.net",
        "earthlink.net",
        "juno.com",
        "netzero.net",
        "optonline.net",
        "roadrunner.com",
        "rr.com",
        "windstream.net",
        "frontier.com",
        "btinternet.com",
        "sky.com",
        "virginmedia.com",
        "talktalk.net",
        "ntlworld.com",
        "blueyonder.co.uk",
        "tiscali.co.uk",
        "plus.net",
        "bigpond.com",
        "bigpond.net.au",
        "optusnet.com.au",
        "iinet.net.au",
        "tpg.com.au",
        "xtra.co.nz",
        "shaw.ca",
        "rogers.com",
        "sympatico.ca",
        "telus.net",
        "videotron.ca",
        "uol.com.br",
        "bol.com.br",
        "terra.com.br",
        "ig.com.br",
        "globo.com",
        "prodigy.net.mx",
        // Privacy relays and aliases
        "privaterelay.appleid.com",
        "mozmail.com",
        "duck.com",
        "passmail.net",
        "simplelogin.com",
        "anonaddy.com",
        "addy.io",
        "relay.firefox.com",
        // Disposable / throwaway. Not a hijack risk in themselves - the owner
        // does control the address - but a workspace built on one is nobody's
        // company, and claiming the domain would rope in every other
        // throwaway user of the same service.
        "mailinator.com",
        "guerrillamail.com",
        "sharklasers.com",
        "10minutemail.com",
        "yopmail.com",
        "trashmail.com",
        "getnada.com",
        "temp-mail.org",
        "tempmail.com",
        "throwawaymail.com",
        "maildrop.cc",
        "dispostable.com",
        "fakeinbox.com",
        "mailnesia.com",
        "spamgourmet.com",
        "moakt.com",
        "emailondeck.com",
    ];

    let lower = domain.to_lowercase();
    PERSONAL_DOMAINS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The providers that were claimed in production before the list was
    /// widened. Each one is a whole consumer provider that a single workspace
    /// had taken ownership of.
    #[test]
    fn known_claimed_providers_are_personal() {
        for domain in ["qq.com", "yandex.ru", "comcast.net", "web.de", "126.com"] {
            assert!(is_builtin_personal_email_domain(domain), "{domain} must not be claimable");
        }
    }

    /// Regional variants are separate domains, and missing one is what let
    /// `yandex.ru` be claimed while `yandex.com` was blocked.
    #[test]
    fn regional_variants_are_personal() {
        for domain in ["yahoo.fr", "yahoo.co.in", "hotmail.de", "live.co.uk", "outlook.fr", "gmx.at"] {
            assert!(is_builtin_personal_email_domain(domain), "{domain} must not be claimable");
        }
    }

    /// A company domain must still be claimable, or colleague-matching stops
    /// working for the case the feature exists to serve.
    #[test]
    fn company_domains_stay_claimable() {
        for domain in ["doubleword.ai", "acme.com", "mycomcast.net", "qq.com.mycompany.io"] {
            assert!(!is_builtin_personal_email_domain(domain), "{domain} must stay claimable");
        }
    }

    #[test]
    fn personal_check_is_case_insensitive() {
        assert!(is_builtin_personal_email_domain("GMAIL.com"));
        assert!(is_builtin_personal_email_domain("QQ.COM"));
    }

    #[test]
    fn email_domain_normalises_and_rejects_empties() {
        assert_eq!(email_domain("bob@Acme.COM"), Some("acme.com".to_string()));
        // Trailing dot is the DNS root; without stripping it, "acme.com." is a
        // second, separate claim on the same company.
        assert_eq!(email_domain("bob@acme.com."), Some("acme.com".to_string()));
        // An empty domain must not reach `find_by_domain`, where `$1 || '~%'`
        // would degrade into a bare `~%` wildcard.
        assert_eq!(email_domain("bob@"), None);
        assert_eq!(email_domain("bob@   "), None);
        assert_eq!(email_domain("no-at-sign"), None);
    }
}
