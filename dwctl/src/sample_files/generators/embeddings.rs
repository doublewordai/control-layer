//! Embeddings sample generator.
//!
//! Generates sample requests for vector embeddings, demonstrating
//! use cases like semantic search, clustering, and RAG applications.

use super::SampleGenerator;
use crate::db::models::deployments::ModelType;
use fusillade::RequestTemplateInput;
use rand::prelude::SliceRandom;
use rand::thread_rng;

/// Generator for embeddings samples.
pub struct EmbeddingsGenerator;

impl EmbeddingsGenerator {
    /// Diverse text samples for embedding generation.
    /// Covers technical, business, and general knowledge domains.
    const SAMPLE_TEXTS: &'static [&'static str] = &[
        // Technical / AI / ML
        "Machine learning is a subset of artificial intelligence that enables systems to learn from data.",
        "Neural networks are computing systems inspired by biological neural networks in the brain.",
        "Deep learning uses multiple layers of neural networks to progressively extract features from data.",
        "Natural language processing enables computers to understand and generate human language.",
        "Computer vision allows machines to interpret and make decisions based on visual data.",
        "Reinforcement learning trains agents to make decisions by rewarding desired behaviors.",
        "Transfer learning leverages pre-trained models to solve new but related problems efficiently.",
        "Transformer architecture revolutionized NLP by enabling parallel processing of sequences.",
        "Attention mechanisms allow models to focus on relevant parts of the input when making predictions.",
        "Generative AI creates new content by learning patterns from existing data.",
        // Software Engineering
        "Microservices architecture decomposes applications into small, independently deployable services.",
        "Containerization packages applications with their dependencies for consistent deployment.",
        "Kubernetes orchestrates containerized workloads across clusters of machines.",
        "CI/CD pipelines automate the building, testing, and deployment of software.",
        "Infrastructure as code manages and provisions computing resources through machine-readable files.",
        "API gateways provide a single entry point for managing and routing API requests.",
        "Message queues enable asynchronous communication between distributed system components.",
        "Database sharding horizontally partitions data across multiple database instances.",
        "Caching strategies reduce latency by storing frequently accessed data in memory.",
        "Load balancers distribute incoming traffic across multiple servers for high availability.",
        // Data and Analytics
        "Vector databases store and efficiently query high-dimensional embedding vectors.",
        "Semantic search finds results based on meaning rather than exact keyword matches.",
        "Retrieval-augmented generation combines search with language models for accurate responses.",
        "Data pipelines automate the flow of data from sources to destinations with transformations.",
        "Feature engineering creates meaningful inputs for machine learning models from raw data.",
        "A/B testing compares two versions to determine which performs better statistically.",
        "Time series analysis examines data points collected over time to identify trends and patterns.",
        "Anomaly detection identifies data points that deviate significantly from expected behavior.",
        "Clustering algorithms group similar data points together based on their characteristics.",
        "Dimensionality reduction simplifies data by reducing the number of input variables.",
        // Business and Product
        "Customer segmentation divides customers into groups based on shared characteristics.",
        "Churn prediction identifies customers likely to stop using a product or service.",
        "Recommendation systems suggest relevant items based on user preferences and behavior.",
        "Sentiment analysis determines the emotional tone behind text content.",
        "Named entity recognition identifies and classifies entities in text like names and locations.",
        "Document classification automatically categorizes documents based on their content.",
        "Question answering systems provide direct answers to natural language questions.",
        "Text summarization condenses long documents into shorter, coherent summaries.",
        "Topic modeling discovers abstract topics that occur in a collection of documents.",
        "Intent classification determines the purpose or goal behind a user's query.",
        // Security and Privacy
        "Zero-trust security assumes no implicit trust and verifies every access request.",
        "Encryption protects data by converting it into an unreadable format without the key.",
        "Authentication verifies the identity of users attempting to access a system.",
        "Authorization determines what actions authenticated users are permitted to perform.",
        "Data anonymization removes personally identifiable information from datasets.",
        "Differential privacy adds noise to data to protect individual privacy in aggregate queries.",
        "Secure multi-party computation allows joint computation without revealing private inputs.",
        "Homomorphic encryption enables computation on encrypted data without decryption.",
        "Federated learning trains models across decentralized data without sharing raw data.",
        "Threat modeling identifies potential security vulnerabilities in system design.",
        // Cloud and Infrastructure
        "Serverless computing executes code in response to events without managing servers.",
        "Edge computing processes data closer to where it is generated for lower latency.",
        "Content delivery networks cache content at locations geographically close to users.",
        "Auto-scaling automatically adjusts computing resources based on current demand.",
        "High availability ensures systems remain operational during component failures.",
        "Disaster recovery plans outline procedures for restoring systems after catastrophic events.",
        "Blue-green deployment reduces downtime by running two identical production environments.",
        "Canary releases gradually roll out changes to a subset of users before full deployment.",
        "Service mesh manages communication between microservices in a distributed system.",
        "Observability provides insight into system behavior through logs, metrics, and traces.",
        // General Knowledge
        "Climate change refers to long-term shifts in global temperatures and weather patterns.",
        "Sustainable development meets present needs without compromising future generations.",
        "Renewable energy comes from sources that naturally replenish like solar and wind.",
        "Biodiversity encompasses the variety of life forms on Earth at all levels.",
        "The scientific method is a systematic approach to investigating phenomena.",
        "Critical thinking involves analyzing and evaluating information to form judgments.",
        "Effective communication conveys ideas clearly and engages the intended audience.",
        "Project management applies knowledge and skills to achieve project objectives.",
        "Design thinking is a human-centered approach to innovation and problem-solving.",
        "Agile methodology emphasizes iterative development and continuous improvement.",
    ];
}

impl SampleGenerator for EmbeddingsGenerator {
    fn name(&self) -> &'static str {
        "Sample: Embeddings"
    }

    fn description(&self) -> &'static str {
        "Vector embedding requests for semantic search, clustering, and RAG applications"
    }

    fn required_model_type(&self) -> ModelType {
        ModelType::Embeddings
    }

    fn required_capabilities(&self) -> &'static [&'static str] {
        &[] // No special capabilities required
    }

    fn generate(&self, model_alias: &str, api_key: &str, endpoint: &str, count: usize) -> Vec<RequestTemplateInput> {
        let mut rng = thread_rng();

        (0..count)
            .map(|i| {
                let text = Self::SAMPLE_TEXTS.choose(&mut rng).unwrap_or(&Self::SAMPLE_TEXTS[0]);

                let body = serde_json::json!({
                    "model": model_alias,
                    "input": text
                });

                RequestTemplateInput {
                    custom_id: Some(format!("embed-{:05}", i)),
                    endpoint: endpoint.to_string(),
                    method: "POST".to_string(),
                    path: "/v1/embeddings".to_string(),
                    model: model_alias.to_string(),
                    api_key: api_key.to_string(),
                    body: body.to_string(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embeddings_generator_metadata() {
        let generator = EmbeddingsGenerator;
        assert_eq!(generator.name(), "Sample: Embeddings");
        assert_eq!(generator.required_model_type(), ModelType::Embeddings);
        assert!(generator.required_capabilities().is_empty());
    }

    #[test]
    fn test_embeddings_generator_output() {
        let generator = EmbeddingsGenerator;
        let requests = generator.generate("text-embedding-ada-002", "test-key", "https://api.example.com", 10);

        assert_eq!(requests.len(), 10);

        for (i, req) in requests.iter().enumerate() {
            assert_eq!(req.custom_id, Some(format!("embed-{:05}", i)));
            assert_eq!(req.method, "POST");
            assert_eq!(req.path, "/v1/embeddings");
            assert_eq!(req.model, "text-embedding-ada-002");
            assert_eq!(req.api_key, "test-key");

            // Verify body is valid JSON with expected structure
            let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            assert_eq!(body["model"], "text-embedding-ada-002");
            assert!(body["input"].is_string());
            assert!(!body["input"].as_str().unwrap().is_empty());
        }
    }
}
