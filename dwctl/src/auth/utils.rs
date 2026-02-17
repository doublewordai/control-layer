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
