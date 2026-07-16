//! Shared screenshot-capture fidelity (M20 D2): viewport presets, image
//! format/quality, and the `Emulation.*` CDP command-building both the
//! host-side driver ([`crate::cdp`] — `browser_render`/`browser_interact`'s
//! live path) and the in-container driver ([`crate::incontainer`] —
//! `ui_screenshot`) drive over the same [`crate::cdp::CdpTransport`] seam.
//! Putting the wire shape here once means a preset serializes IDENTICALLY
//! for both call sites, and there is exactly one place that knows it.
//!
//! ## Back-compat is the whole point of this module's shape
//!
//! [`CaptureOptions::legacy_full_page`] reproduces the CDP params `cdp.rs`
//! hard-coded before this card (`captureBeyondViewport: true`, PNG, no
//! `Emulation.*` call at all) byte-for-byte, and is [`CaptureOptions`]'s
//! [`Default`] — so a `browser_render`/`browser_interact` call that passes no
//! viewport/format args produces the EXACT pre-D2 CDP command sequence.
//! [`CaptureOptions::ui_screenshot_default`] mirrors D1's fixed windowed
//! 1280x800 PNG default the same way. See
//! `docs/plans/m20-coding-and-design-capability-program.md` card D2.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::cdp::CdpTransport;
use crate::error::BrowserError;

/// A named viewport preset. `Desktop` is D1's fixed default (1280x800, no
/// mobile emulation) — kept byte-identical here. `Mobile` is D2's ONE mobile
/// preset (390x844, touch + mobile UA) — deliberately not a device matrix
/// (rejected alternative; see the M20 plan's "Deferred/rejected").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewportPreset {
    Desktop,
    Mobile,
}

impl ViewportPreset {
    /// D1's fixed windowed default — do not change without re-verifying every
    /// byte-compat test that pins it.
    pub const DESKTOP_WIDTH: u32 = 1280;
    pub const DESKTOP_HEIGHT: u32 = 800;
    /// D2's one mobile preset.
    pub const MOBILE_WIDTH: u32 = 390;
    pub const MOBILE_HEIGHT: u32 = 844;

    /// A plausible modern mobile Chrome UA for `Network.setUserAgentOverride`.
    pub const MOBILE_USER_AGENT: &'static str = "Mozilla/5.0 (Linux; Android 13; Pixel 7) \
         AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36";

    /// Pixel dimensions for this preset.
    #[must_use]
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            ViewportPreset::Desktop => (Self::DESKTOP_WIDTH, Self::DESKTOP_HEIGHT),
            ViewportPreset::Mobile => (Self::MOBILE_WIDTH, Self::MOBILE_HEIGHT),
        }
    }

    #[must_use]
    pub fn is_mobile(self) -> bool {
        matches!(self, ViewportPreset::Mobile)
    }

    /// Parse the tool-arg token (`"desktop"` | `"mobile"`, case-insensitive).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "desktop" => Ok(ViewportPreset::Desktop),
            "mobile" => Ok(ViewportPreset::Mobile),
            other => Err(format!(
                "unknown viewport preset `{other}` (expected `desktop` | `mobile`)"
            )),
        }
    }

    /// The `Emulation.setDeviceMetricsOverride` params for this preset. For
    /// `Desktop` this is byte-identical to D1's hard-coded inline call
    /// (`width: 1280, height: 800, deviceScaleFactor: 1, mobile: false`).
    #[must_use]
    pub fn device_metrics_params(self) -> Value {
        let (width, height) = self.dimensions();
        json!({
            "width": width,
            "height": height,
            "deviceScaleFactor": if self.is_mobile() { 2 } else { 1 },
            "mobile": self.is_mobile(),
        })
    }
}

/// A screenshot's image encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// Parse the tool-arg token (`"png"` | `"jpeg"`/`"jpg"`, case-insensitive).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "png" => Ok(ImageFormat::Png),
            "jpeg" | "jpg" => Ok(ImageFormat::Jpeg),
            other => Err(format!(
                "unknown format `{other}` (expected `png` | `jpeg`)"
            )),
        }
    }

    /// The literal CDP `Page.captureScreenshot` `format` value.
    #[must_use]
    pub fn as_cdp_str(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
        }
    }

    /// The saved-file extension.
    #[must_use]
    pub fn file_extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }

    /// The MIME type for a returned image content block.
    #[must_use]
    pub fn mime_type(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        }
    }
}

/// JPEG quality used by the automatic oversize-capture downgrade
/// (`ui_screenshot`'s size-safety retry, [`crate::incontainer::capture_with_size_safety`]).
pub const DOWNGRADE_JPEG_QUALITY: u8 = 70;

/// Full capture fidelity for one `Page.captureScreenshot` call: viewport
/// sizing (`None` skips `Emulation.*` entirely — the pre-D2 legacy path),
/// full-page vs. windowed, and image format/quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureOptions {
    /// `None` skips ALL `Emulation.*`/`Network.setUserAgentOverride` calls —
    /// the exact pre-D2 `browser_render` behavior (whatever window size the
    /// container's chromium was launched with).
    #[serde(default)]
    pub viewport: Option<ViewportPreset>,
    /// Maps to `Page.captureScreenshot`'s `captureBeyondViewport`.
    pub full_page: bool,
    pub format: ImageFormat,
    /// JPEG quality (1-100). Ignored for PNG.
    #[serde(default)]
    pub quality: Option<u8>,
}

impl CaptureOptions {
    /// The pre-D2 `browser_render`/`browser_interact` hard-coded default: no
    /// viewport override, full-page PNG. BYTE IDENTICAL to the CDP params
    /// `cdp.rs` sent before this card — this is [`CaptureOptions`]'s
    /// [`Default`], so an omitted `capture` field on the wire reproduces it.
    #[must_use]
    pub fn legacy_full_page() -> Self {
        Self {
            viewport: None,
            full_page: true,
            format: ImageFormat::Png,
            quality: None,
        }
    }

    /// D1's fixed `ui_screenshot` default: windowed 1280x800 PNG. BYTE
    /// IDENTICAL to D1's hard-coded capture (`incontainer.rs` pre-D2).
    #[must_use]
    pub fn ui_screenshot_default() -> Self {
        Self {
            viewport: Some(ViewportPreset::Desktop),
            full_page: false,
            format: ImageFormat::Png,
            quality: None,
        }
    }

    /// The size-safety auto-downgrade: same viewport/full-page framing,
    /// re-encoded as jpeg at [`DOWNGRADE_JPEG_QUALITY`].
    #[must_use]
    pub fn downgraded_to_jpeg(&self) -> Self {
        Self {
            format: ImageFormat::Jpeg,
            quality: Some(DOWNGRADE_JPEG_QUALITY),
            ..self.clone()
        }
    }

    /// The `Page.captureScreenshot` params for this capture. `quality` is
    /// included only for jpeg (mirrors the CDP contract, which ignores it for
    /// png) and only when explicitly set — omitted, CDP applies its own
    /// default.
    #[must_use]
    pub fn capture_params(&self) -> Value {
        let mut map = Map::new();
        map.insert("format".to_string(), json!(self.format.as_cdp_str()));
        map.insert("captureBeyondViewport".to_string(), json!(self.full_page));
        if self.format == ImageFormat::Jpeg {
            if let Some(q) = self.quality {
                map.insert("quality".to_string(), json!(q));
            }
        }
        Value::Object(map)
    }
}

impl Default for CaptureOptions {
    /// Defaults to the legacy full-page behavior — an omitted `capture` field
    /// on the wire (`#[serde(default)]`) reproduces pre-D2 `browser_render`
    /// output exactly.
    fn default() -> Self {
        Self::legacy_full_page()
    }
}

/// Apply `viewport`'s device-metrics override + (mobile only) touch
/// emulation + UA override, or do nothing at all when `viewport` is `None`.
/// This is the exact CDP command sequence both [`crate::cdp::CdpBrowserDriver`]
/// and [`crate::incontainer::capture`] drive before `Page.captureScreenshot`,
/// so the wire shape is identical for both call sites. For [`ViewportPreset::Desktop`]
/// this issues ONLY the metrics-override call (byte-identical to D1's
/// pre-D2 inline call); the touch/UA calls are mobile-only so the desktop
/// preset's CDP sequence is unchanged from D1.
pub(crate) async fn apply_viewport(
    transport: &dyn CdpTransport,
    viewport: Option<ViewportPreset>,
) -> Result<(), BrowserError> {
    let Some(preset) = viewport else {
        return Ok(());
    };
    transport
        .send(
            "Emulation.setDeviceMetricsOverride",
            preset.device_metrics_params(),
        )
        .await?;
    if preset.is_mobile() {
        transport
            .send(
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": true }),
            )
            .await?;
        transport
            .send(
                "Network.setUserAgentOverride",
                json!({ "userAgent": ViewportPreset::MOBILE_USER_AGENT }),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    // ── ViewportPreset ────────────────────────────────────────────────────

    #[test]
    fn desktop_dimensions_match_d1_default() {
        assert_eq!(
            ViewportPreset::Desktop.dimensions(),
            (
                ViewportPreset::DESKTOP_WIDTH,
                ViewportPreset::DESKTOP_HEIGHT
            )
        );
        assert_eq!(ViewportPreset::Desktop.dimensions(), (1280, 800));
        assert!(!ViewportPreset::Desktop.is_mobile());
    }

    #[test]
    fn mobile_dimensions_are_the_one_d2_preset() {
        assert_eq!(ViewportPreset::Mobile.dimensions(), (390, 844));
        assert!(ViewportPreset::Mobile.is_mobile());
    }

    #[test]
    fn viewport_parse_is_case_insensitive() {
        assert_eq!(
            ViewportPreset::parse("Desktop").unwrap(),
            ViewportPreset::Desktop
        );
        assert_eq!(
            ViewportPreset::parse("MOBILE").unwrap(),
            ViewportPreset::Mobile
        );
        assert!(ViewportPreset::parse("tablet").is_err());
    }

    #[test]
    fn desktop_device_metrics_params_match_d1_inline_call() {
        let params = ViewportPreset::Desktop.device_metrics_params();
        assert_eq!(
            params,
            json!({ "width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": false })
        );
    }

    #[test]
    fn mobile_device_metrics_params_set_mobile_flag() {
        let params = ViewportPreset::Mobile.device_metrics_params();
        assert_eq!(params["width"], json!(390));
        assert_eq!(params["height"], json!(844));
        assert_eq!(params["mobile"], json!(true));
    }

    // ── ImageFormat ────────────────────────────────────────────────────────

    #[test]
    fn format_parse_accepts_jpeg_and_jpg() {
        assert_eq!(ImageFormat::parse("png").unwrap(), ImageFormat::Png);
        assert_eq!(ImageFormat::parse("JPEG").unwrap(), ImageFormat::Jpeg);
        assert_eq!(ImageFormat::parse("jpg").unwrap(), ImageFormat::Jpeg);
        assert!(ImageFormat::parse("gif").is_err());
    }

    #[test]
    fn format_extensions_and_mime_types() {
        assert_eq!(ImageFormat::Png.file_extension(), "png");
        assert_eq!(ImageFormat::Jpeg.file_extension(), "jpg");
        assert_eq!(ImageFormat::Png.mime_type(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime_type(), "image/jpeg");
    }

    // ── CaptureOptions ───────────────────────────────────────────────────

    #[test]
    fn legacy_default_is_captureoptions_default() {
        assert_eq!(
            CaptureOptions::default(),
            CaptureOptions::legacy_full_page()
        );
    }

    #[test]
    fn legacy_full_page_capture_params_match_pre_d2_hardcoded_json() {
        // The exact literal `cdp.rs` sent before this card:
        // json!({ "format": "png", "captureBeyondViewport": true })
        let opts = CaptureOptions::legacy_full_page();
        assert_eq!(
            opts.capture_params(),
            json!({ "format": "png", "captureBeyondViewport": true })
        );
        assert!(
            opts.viewport.is_none(),
            "legacy path must skip Emulation.* entirely"
        );
    }

    #[test]
    fn ui_screenshot_default_capture_params_match_pre_d2_d1_json() {
        // The exact literal D1's `incontainer.rs` sent before this card:
        // json!({ "format": "png", "captureBeyondViewport": false })
        let opts = CaptureOptions::ui_screenshot_default();
        assert_eq!(
            opts.capture_params(),
            json!({ "format": "png", "captureBeyondViewport": false })
        );
        assert_eq!(opts.viewport, Some(ViewportPreset::Desktop));
    }

    #[test]
    fn jpeg_capture_params_include_quality_when_set() {
        let opts = CaptureOptions {
            viewport: None,
            full_page: false,
            format: ImageFormat::Jpeg,
            quality: Some(70),
        };
        assert_eq!(
            opts.capture_params(),
            json!({ "format": "jpeg", "captureBeyondViewport": false, "quality": 70 })
        );
    }

    #[test]
    fn jpeg_capture_params_omit_quality_when_unset() {
        let opts = CaptureOptions {
            viewport: None,
            full_page: true,
            format: ImageFormat::Jpeg,
            quality: None,
        };
        let params = opts.capture_params();
        assert!(params.get("quality").is_none());
    }

    #[test]
    fn png_capture_params_never_include_quality_even_if_set() {
        let opts = CaptureOptions {
            viewport: None,
            full_page: true,
            format: ImageFormat::Png,
            quality: Some(50), // nonsensical for png — must be dropped, not sent to CDP
        };
        let params = opts.capture_params();
        assert!(params.get("quality").is_none());
    }

    #[test]
    fn downgraded_to_jpeg_keeps_viewport_and_full_page() {
        let opts = CaptureOptions {
            viewport: Some(ViewportPreset::Mobile),
            full_page: true,
            format: ImageFormat::Png,
            quality: None,
        };
        let down = opts.downgraded_to_jpeg();
        assert_eq!(down.viewport, Some(ViewportPreset::Mobile));
        assert!(down.full_page);
        assert_eq!(down.format, ImageFormat::Jpeg);
        assert_eq!(down.quality, Some(DOWNGRADE_JPEG_QUALITY));
    }

    // ── apply_viewport (mock CdpTransport — D1's mock seam) ──────────────

    #[derive(Default)]
    struct RecordingTransport {
        calls: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl CdpTransport for RecordingTransport {
        async fn send(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            Ok(json!({}))
        }
        async fn wait_for_load(&self, _timeout: Duration) -> Result<(), BrowserError> {
            Ok(())
        }
        fn redirect_hops(&self) -> Vec<String> {
            Vec::new()
        }
        fn main_status(&self) -> Option<u16> {
            Some(200)
        }
    }

    #[tokio::test]
    async fn apply_viewport_none_issues_no_cdp_calls() {
        let transport = RecordingTransport::default();
        apply_viewport(&transport, None).await.unwrap();
        assert!(
            transport.calls.lock().unwrap().is_empty(),
            "legacy (None) path must not touch Emulation.* at all"
        );
    }

    #[tokio::test]
    async fn apply_viewport_desktop_issues_only_device_metrics_call() {
        let transport = RecordingTransport::default();
        apply_viewport(&transport, Some(ViewportPreset::Desktop))
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "desktop preset must match D1's single inline call, got {calls:?}"
        );
        assert_eq!(calls[0].0, "Emulation.setDeviceMetricsOverride");
    }

    #[tokio::test]
    async fn apply_viewport_mobile_issues_metrics_touch_and_ua_calls() {
        let transport = RecordingTransport::default();
        apply_viewport(&transport, Some(ViewportPreset::Mobile))
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            vec![
                "Emulation.setDeviceMetricsOverride",
                "Emulation.setTouchEmulationEnabled",
                "Network.setUserAgentOverride",
            ]
        );
        assert_eq!(calls[1].1, json!({ "enabled": true }));
        assert_eq!(
            calls[2].1,
            json!({ "userAgent": ViewportPreset::MOBILE_USER_AGENT })
        );
    }
}
