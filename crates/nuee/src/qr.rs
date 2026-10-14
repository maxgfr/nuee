//! QR codes for links: unicode half blocks in the terminal, SVG files on request.

use anyhow::Context as _;
use qrcode::render::{svg, unicode};
use qrcode::{EcLevel, QrCode};

fn code(text: &str) -> anyhow::Result<QrCode> {
    // Links are uppercase base32 after the fragment; low correction keeps the code small.
    QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L)
        .context("the link is too long for a QR code")
}

/// The QR code as text, two modules per character cell, with a quiet zone. Colors are
/// inverted (light modules drawn) so it reads on dark terminals.
pub fn terminal(text: &str) -> anyhow::Result<String> {
    let code = code(text)?;
    Ok(code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

/// The QR code as a standalone SVG document.
pub fn svg(text: &str) -> anyhow::Result<String> {
    let code = code(text)?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .build())
}
