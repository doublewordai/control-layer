//! Embedded static assets for serving the frontend application.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct Assets;
