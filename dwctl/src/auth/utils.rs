//! Authentication utility functions.

use rand::prelude::RngExt;
use rand::rng;

/// Generate a random inference-themed display name
/// Format: "{adjective} {noun} {4-digit number}"
/// Example: "Swift Inference 4729"
pub fn generate_random_display_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "Swift",
        "Neural",
        "Deep",
        "Smart",
        "Quantum",
        "Adaptive",
        "Dynamic",
        "Logical",
        "Efficient",
        "Precise",
        "Optimized",
        "Parallel",
        "Recursive",
        "Semantic",
        "Synthetic",
    ];

    const NOUNS: &[&str] = &[
        "Inference",
        "Network",
        "Model",
        "Agent",
        "Processor",
        "Analyzer",
        "Engine",
        "System",
        "Predictor",
        "Learner",
        "Classifier",
        "Transformer",
        "Encoder",
        "Decoder",
        "Reasoning",
    ];

    let mut rng = rng();
    let adjective = ADJECTIVES[rng.random_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.random_range(0..NOUNS.len())];
    let number = rng.random_range(1000..10000);

    format!("{} {} {}", adjective, noun, number)
}

/// Extract the domain part from an email address.
/// Returns `None` if the email doesn't contain an `@`.
pub fn email_domain(email: &str) -> Option<&str> {
    email.rsplit_once('@').map(|(_, domain)| domain)
}

/// Returns `true` if the domain belongs to a personal/free email provider
/// where auto-org creation would be inappropriate.
pub fn is_personal_email_domain(domain: &str) -> bool {
    const PERSONAL_DOMAINS: &[&str] = &[
        "gmail.com",
        "googlemail.com",
        "hotmail.com",
        "hotmail.co.uk",
        "live.com",
        "outlook.com",
        "msn.com",
        "yahoo.com",
        "yahoo.co.uk",
        "yahoo.co.jp",
        "ymail.com",
        "aol.com",
        "protonmail.com",
        "proton.me",
        "icloud.com",
        "me.com",
        "mac.com",
        "mail.com",
        "zoho.com",
        "yandex.com",
        "gmx.com",
        "gmx.de",
        "fastmail.com",
        "tutanota.com",
        "tuta.com",
    ];

    let lower = domain.to_lowercase();
    PERSONAL_DOMAINS.contains(&lower.as_str())
}
