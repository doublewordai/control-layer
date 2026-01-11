//! Vision sample generator.
//!
//! Generates sample requests for image analysis, demonstrating
//! vision model capabilities like image description, OCR, and object detection.

use super::SampleGenerator;
use crate::db::models::deployments::ModelType;
use fusillade::RequestTemplateInput;
use rand::prelude::SliceRandom;
use rand::thread_rng;

/// Generator for vision/image analysis samples.
pub struct VisionGenerator;

impl VisionGenerator {
    /// Sample image URLs using public placeholder images.
    /// These use Lorem Picsum which provides freely usable placeholder images.
    const SAMPLE_IMAGE_URLS: &'static [&'static str] = &[
        "https://picsum.photos/id/1/800/600",   // Laptop on desk
        "https://picsum.photos/id/10/800/600",  // Forest
        "https://picsum.photos/id/20/800/600",  // Coffee cup
        "https://picsum.photos/id/26/800/600",  // Coastline
        "https://picsum.photos/id/37/800/600",  // Mountain and lake
        "https://picsum.photos/id/48/800/600",  // Grass field
        "https://picsum.photos/id/65/800/600",  // Art supplies
        "https://picsum.photos/id/96/800/600",  // Desk workspace
        "https://picsum.photos/id/119/800/600", // Flowers
        "https://picsum.photos/id/160/800/600", // Architecture
        "https://picsum.photos/id/180/800/600", // Food
        "https://picsum.photos/id/200/800/600", // City lights
        "https://picsum.photos/id/237/800/600", // Dog
        "https://picsum.photos/id/250/800/600", // Beach
        "https://picsum.photos/id/292/800/600", // Building
        "https://picsum.photos/id/318/800/600", // Technology
        "https://picsum.photos/id/365/800/600", // Nature
        "https://picsum.photos/id/401/800/600", // Abstract
        "https://picsum.photos/id/433/800/600", // Urban
        "https://picsum.photos/id/488/800/600", // Wildlife
    ];

    /// Vision-specific prompts for image analysis.
    const VISION_PROMPTS: &'static [&'static str] = &[
        // Description tasks
        "Describe this image in detail. Include information about the setting, subjects, colors, and mood.",
        "Write a concise caption for this image suitable for social media.",
        "Describe the composition and visual elements of this photograph.",
        "What story does this image tell? Describe the narrative you see.",
        "Provide a detailed description suitable for someone who cannot see the image.",
        // Accessibility
        "Generate alt text for this image that would be helpful for screen readers.",
        "Write an accessibility description focusing on the most important visual elements.",
        "Create a brief but informative alt text for web accessibility compliance.",
        // Analysis tasks
        "What objects can you identify in this image? List them with their approximate positions.",
        "Analyze the color palette used in this image. What are the dominant colors?",
        "Describe the lighting in this image. Is it natural or artificial? What mood does it create?",
        "What is the focal point of this image and how does the composition draw attention to it?",
        // Categorization
        "What category would this image belong to? (e.g., nature, technology, food, etc.)",
        "Is this image suitable for a professional website? Explain your reasoning.",
        "Rate the quality of this image for stock photography use and explain why.",
        "What emotions or feelings does this image evoke?",
        // Content moderation
        "Does this image contain any text? If so, transcribe it.",
        "Describe any people visible in this image without identifying specific individuals.",
        "Are there any potential safety concerns or sensitive content in this image?",
        "Would this image be appropriate for all ages? Explain.",
        // Creative applications
        "Suggest three different headlines that could accompany this image in an article.",
        "What products or services could this image be used to advertise?",
        "Describe how this image could be improved for commercial use.",
        "What complementary images would pair well with this one in a gallery?",
    ];

    /// System prompts for vision tasks.
    const SYSTEM_PROMPTS: &'static [&'static str] = &[
        "You are an image analysis assistant providing detailed, accurate descriptions.",
        "You are a professional photo editor analyzing images for quality and composition.",
        "You are an accessibility expert creating descriptions for visually impaired users.",
        "You are a content moderator reviewing images for appropriateness.",
        "You are a creative director evaluating images for marketing campaigns.",
    ];
}

impl SampleGenerator for VisionGenerator {
    fn name(&self) -> &'static str {
        "Sample: Vision"
    }

    fn description(&self) -> &'static str {
        "Image analysis requests demonstrating vision capabilities like description, OCR, and object detection"
    }

    fn required_model_type(&self) -> ModelType {
        ModelType::Chat
    }

    fn required_capabilities(&self) -> &'static [&'static str] {
        &["vision"]
    }

    fn generate(&self, model_alias: &str, api_key: &str, endpoint: &str, count: usize) -> Vec<RequestTemplateInput> {
        let mut rng = thread_rng();

        (0..count)
            .map(|i| {
                let image_url = Self::SAMPLE_IMAGE_URLS.choose(&mut rng).unwrap_or(&Self::SAMPLE_IMAGE_URLS[0]);
                let prompt = Self::VISION_PROMPTS.choose(&mut rng).unwrap_or(&Self::VISION_PROMPTS[0]);
                let system = Self::SYSTEM_PROMPTS.choose(&mut rng).unwrap_or(&Self::SYSTEM_PROMPTS[0]);

                let body = serde_json::json!({
                    "model": model_alias,
                    "messages": [
                        {"role": "system", "content": system},
                        {
                            "role": "user",
                            "content": [
                                {
                                    "type": "image_url",
                                    "image_url": {
                                        "url": image_url
                                    }
                                },
                                {
                                    "type": "text",
                                    "text": prompt
                                }
                            ]
                        }
                    ],
                    "max_tokens": 1024
                });

                RequestTemplateInput {
                    custom_id: Some(format!("vision-{:05}", i)),
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
    fn test_vision_generator_metadata() {
        let generator = VisionGenerator;
        assert_eq!(generator.name(), "Sample: Vision");
        assert_eq!(generator.required_model_type(), ModelType::Chat);
        assert_eq!(generator.required_capabilities(), &["vision"]);
    }

    #[test]
    fn test_vision_generator_output() {
        let generator = VisionGenerator;
        let requests = generator.generate("gpt-4-vision", "test-key", "https://api.example.com", 10);

        assert_eq!(requests.len(), 10);

        for (i, req) in requests.iter().enumerate() {
            assert_eq!(req.custom_id, Some(format!("vision-{:05}", i)));
            assert_eq!(req.method, "POST");
            assert_eq!(req.path, "/v1/chat/completions");
            assert_eq!(req.model, "gpt-4-vision");
            assert_eq!(req.api_key, "test-key");

            // Verify body is valid JSON with expected structure
            let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            assert_eq!(body["model"], "gpt-4-vision");
            assert!(body["messages"].is_array());

            // Check the user message has vision content format
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 2);

            let user_message = &messages[1];
            assert_eq!(user_message["role"], "user");
            assert!(user_message["content"].is_array());

            let content = user_message["content"].as_array().unwrap();
            assert_eq!(content.len(), 2);
            assert_eq!(content[0]["type"], "image_url");
            assert_eq!(content[1]["type"], "text");
        }
    }
}
