//! Chat completion sample generator.
//!
//! Generates sample requests for basic chat completions, demonstrating
//! various text generation tasks like writing, analysis, and Q&A.

use super::SampleGenerator;
use crate::db::models::deployments::ModelType;
use fusillade::RequestTemplateInput;
use rand::prelude::SliceRandom;
use rand::thread_rng;

/// Generator for basic chat completion samples.
pub struct ChatGenerator;

impl ChatGenerator {
    /// Varied user prompts covering different chat completion use cases.
    const SAMPLE_PROMPTS: &'static [&'static str] = &[
        // Writing assistance
        "Write a professional email declining a meeting invitation politely.",
        "Draft a brief product description for a wireless noise-canceling headphone.",
        "Create a compelling introduction paragraph for a blog post about sustainable living.",
        "Write a thank you note to a colleague who helped with a project.",
        "Compose a social media post announcing a new product launch.",
        "Draft a short bio for a software engineer's LinkedIn profile.",
        "Write a friendly reminder email about an upcoming deadline.",
        "Create a catchy tagline for a fitness app.",
        "Write an apology email for a delayed shipment.",
        "Draft a cover letter opening paragraph for a marketing position.",
        // Analysis and explanation
        "Explain the concept of machine learning to a 10-year-old.",
        "Summarize the key differences between REST and GraphQL APIs.",
        "What are the pros and cons of microservices architecture?",
        "Explain how blockchain technology ensures data integrity.",
        "Describe the main principles of object-oriented programming.",
        "What is the difference between synchronous and asynchronous programming?",
        "Explain the CAP theorem in distributed systems.",
        "Summarize the benefits of test-driven development.",
        "What are the key considerations when designing a database schema?",
        "Explain the concept of eventual consistency.",
        // Q&A and knowledge
        "What are best practices for writing clean, maintainable code?",
        "How does garbage collection work in modern programming languages?",
        "What are the common causes of memory leaks in applications?",
        "Explain the difference between OAuth 2.0 and JWT.",
        "What strategies can improve API response times?",
        "How do content delivery networks improve website performance?",
        "What are the key metrics to monitor in a production system?",
        "Explain the concept of infrastructure as code.",
        "What are the benefits of containerization with Docker?",
        "How does load balancing improve application scalability?",
        // Creative tasks
        "Generate three creative names for a productivity app.",
        "Write a haiku about debugging code.",
        "Create a metaphor explaining cloud computing.",
        "Suggest five topics for a tech podcast episode.",
        "Write a brief story about a robot learning to paint.",
        "Create a fictional conversation between two AI assistants.",
        "Generate a list of creative team building activity ideas.",
        "Write a limerick about software deadlines.",
        "Suggest creative ways to visualize data in a dashboard.",
        "Create an analogy explaining API rate limiting.",
        // Business and strategy
        "What factors should be considered when choosing a tech stack?",
        "How can technical debt be effectively managed?",
        "What are key considerations for GDPR compliance in software?",
        "Explain the build vs buy decision for software features.",
        "What strategies help with successful remote team collaboration?",
        "How should companies approach legacy system modernization?",
        "What are effective code review practices?",
        "Explain the importance of observability in modern systems.",
        "What factors affect API versioning strategy?",
        "How can development teams improve deployment frequency?",
    ];

    /// System prompts to add variety to the requests.
    const SYSTEM_PROMPTS: &'static [&'static str] = &[
        "You are a helpful assistant.",
        "You are a professional technical writer.",
        "You are an experienced software architect.",
        "You are a friendly and concise assistant.",
        "You are a business communication expert.",
        "You are a creative content strategist.",
        "You are a senior developer mentor.",
        "You are a technical documentation specialist.",
    ];
}

impl SampleGenerator for ChatGenerator {
    fn name(&self) -> &'static str {
        "Sample: Chat Completions"
    }

    fn description(&self) -> &'static str {
        "Basic chat completion requests demonstrating text generation, writing assistance, and Q&A"
    }

    fn required_model_type(&self) -> ModelType {
        ModelType::Chat
    }

    fn required_capabilities(&self) -> &'static [&'static str] {
        &[] // No special capabilities required
    }

    fn generate(&self, model_alias: &str, api_key: &str, endpoint: &str, count: usize) -> Vec<RequestTemplateInput> {
        let mut rng = thread_rng();

        (0..count)
            .map(|i| {
                let prompt = Self::SAMPLE_PROMPTS.choose(&mut rng).unwrap_or(&Self::SAMPLE_PROMPTS[0]);
                let system = Self::SYSTEM_PROMPTS.choose(&mut rng).unwrap_or(&Self::SYSTEM_PROMPTS[0]);

                let body = serde_json::json!({
                    "model": model_alias,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": prompt}
                    ],
                    "max_tokens": 1024
                });

                RequestTemplateInput {
                    custom_id: Some(format!("chat-{:05}", i)),
                    endpoint: endpoint.to_string(),
                    method: "POST".to_string(),
                    path: "/v1/chat/completions".to_string(),
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
    fn test_chat_generator_metadata() {
        let generator = ChatGenerator;
        assert_eq!(generator.name(), "Sample: Chat Completions");
        assert_eq!(generator.required_model_type(), ModelType::Chat);
        assert!(generator.required_capabilities().is_empty());
    }

    #[test]
    fn test_chat_generator_output() {
        let generator = ChatGenerator;
        let requests = generator.generate("gpt-4", "test-key", "https://api.example.com", 10);

        assert_eq!(requests.len(), 10);

        for (i, req) in requests.iter().enumerate() {
            assert_eq!(req.custom_id, Some(format!("chat-{:05}", i)));
            assert_eq!(req.method, "POST");
            assert_eq!(req.path, "/v1/chat/completions");
            assert_eq!(req.model, "gpt-4");
            assert_eq!(req.api_key, "test-key");
            assert_eq!(req.endpoint, "https://api.example.com");

            // Verify body is valid JSON with expected structure
            let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            assert_eq!(body["model"], "gpt-4");
            assert!(body["messages"].is_array());
            assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        }
    }
}
