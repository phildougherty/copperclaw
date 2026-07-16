//! `ui_screenshot`: take a screenshot of the agent's OWN running app using a
//! local, in-container headless chromium (M20 D1 — see
//! `docs/plans/m20-coding-and-design-capability-program.md` decision (a)).
//!
//! This closes the vision-loop write path the M18 V4 host-side wiring never
//! actually had a producer for: `browser_render`/`browser_interact` need a
//! Docker socket the in-container runner does not have
//! (`crate::tools::browser_render` §"host path is stranded"), so in every
//! default deployment the agent has never once seen its own UI mid-build.
//! `ui_screenshot` runs the CDP driver **inside the session container
//! itself** (`copperclaw_browser::incontainer`), driving the prototyping
//! image's already-baked chromium over loopback — no container spawn, no
//! Docker socket, no host round-trip.
//!
//! ## Security posture
//!
//!   * **Loopback-only, hard-refused otherwise.** `url` must resolve to
//!     `127.0.0.1` / `::1` / `localhost` (case-insensitive). Anything else is
//!     refused with a hint pointing at `browser_render` — this tool is
//!     deliberately NOT a web-browsing capability.
//!   * **No new privilege (the registration-default argument).** The agent
//!     already has an arbitrary `shell` tool and this same chromium binary
//!     reachable from it in the prototyping image; this tool does not widen
//!     egress, does not add a new sandbox escape surface (see
//!     `copperclaw_browser::incontainer`'s module docs for the
//!     `--no-sandbox` rationale), and — because it is loopback-only — cannot
//!     reach the LAN, the host, or the public internet. That is the argument
//!     for registering it by DEFAULT in the Coding/Full profiles (the one
//!     default change in M20; recorded again in this crate's PR/commit).
//!   * **Not tainted untrusted.** Unlike `browser_render` (a general
//!     web-browsing tool over attacker-influenceable content), the page this
//!     tool screenshots is the agent's OWN in-progress build, reachable only
//!     on loopback. The IMAGE block itself follows the same path as
//!     `view_image` (which also does not taint) — no page-derived TEXT is
//!     ever added to the transcript by this tool, only a trusted,
//!     host-generated save path. If a future extension (e.g. M20 D5's
//!     `ui_inspect` console-error surfacing) adds page-originated TEXT to the
//!     transcript, THAT call must `mark_untrusted_context` — this tool does
//!     not need to because it doesn't.
//!   * **Minimal-profile degradation.** Chromium is probed for at CALL TIME
//!     (`copperclaw_browser::incontainer::find_chromium_binary`); its
//!     absence (the minimal image profile) returns one clean, actionable
//!     `ToolError::Validation` naming the prototyping profile — never a
//!     crash or a hang.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::context::{ToolContext, bytes_b64};
use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args};
use copperclaw_browser::{CaptureOptions, ImageFormat, ViewportPreset};
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

/// Cap on the image this tool will attach. Mirrors `view_image`'s cap
/// (`crate::tools::view_image::MAX_IMAGE_BYTES`): the base64 lives in
/// conversation history until compaction, so an oversized image just burns
/// context budget for no vision benefit. The default 1280x800 windowed
/// capture stays comfortably under this in practice; when it isn't (e.g. a
/// tall page even at 1280x800, or `full_page: true`), [`copperclaw_browser::
/// capture_with_size_safety`] automatically retries once as a lower-fidelity
/// jpeg (M20 D2) rather than erroring outright.
const MAX_SCREENSHOT_BYTES: u64 = copperclaw_browser::SIZE_SAFETY_CAP_BYTES;

/// Navigation/load timeout for the in-container chromium.
const NAV_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
struct Input {
    /// Must be a loopback URL (127.0.0.1 / `::1` / localhost). Anything else
    /// is refused — use `browser_render` for real web browsing.
    url: String,
    /// Extra fixed delay (ms) after load before capturing, e.g. to let a
    /// client-side render settle. Clamped to
    /// [`copperclaw_browser::UI_SCREENSHOT_MAX_WAIT_MS`].
    #[serde(default)]
    wait_ms: Option<u64>,
    /// Poll until this CSS selector exists before capturing.
    #[serde(default)]
    wait_for_selector: Option<String>,
    /// The project directory (e.g. the same path you passed as `cwd` to
    /// `shell`) whose `.copperclaw/screenshots/` the PNG is saved under.
    /// Defaults to the data root's own `.copperclaw/screenshots/` when
    /// omitted.
    #[serde(default)]
    project: Option<String>,
    /// M20 D2: `desktop` (1280x800, DEFAULT — byte-identical to D1's fixed
    /// viewport) | `mobile` (390x844, touch + mobile UA). ONE mobile preset,
    /// not a device matrix.
    #[serde(default)]
    viewport: Option<String>,
    /// M20 D2: capture the whole scrollable page instead of just the
    /// viewport. Defaults to `false` (D1's windowed behavior — full-page
    /// would blow the image-attachment size cap on a long page).
    #[serde(default)]
    full_page: Option<bool>,
    /// M20 D2: `png` (default) | `jpeg`.
    #[serde(default)]
    format: Option<String>,
    /// M20 D2: jpeg quality (1-100). Ignored for png.
    #[serde(default)]
    quality: Option<u8>,
}

pub fn schema() -> Tool {
    make_tool(
        "ui_screenshot",
        "Take a screenshot of YOUR OWN running app (e.g. a vite dev server you just started) \
         using a local, in-container headless chromium — so you can actually SEE the UI you're \
         building and iterate on it (pair with `load_skill(\"frontend-design\")` for the \
         critique checklist). `url` MUST be a loopback address (127.0.0.1 / ::1 / localhost) — \
         this is NOT a general web-browsing tool; for a real external site use `browser_render` \
         instead. Defaults to a 1280x800 WINDOWED viewport (not full-page — full-page would blow \
         the image-attachment size cap) and returns the image directly, plus the path it was \
         saved to under `.copperclaw/screenshots/` so you can `send_file` it later. Optional \
         args: `viewport` (`desktop` default | `mobile` — 390x844 with touch + mobile UA, for \
         checking responsive layout), `full_page` (default false), `format` (`png` default | \
         `jpeg`, with optional `quality`). If a capture ever exceeds the size cap, it is \
         automatically retried once as a lower-fidelity jpeg and the response notes the \
         downgrade — it never just errors. Requires chromium, which is baked into the \
         `prototyping` image profile; on a minimal-profile container this returns a clear, \
         actionable error instead of failing silently.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url": { "type": "string", "minLength": 1 },
                "wait_ms": { "type": ["integer", "null"], "minimum": 0, "maximum": 10000 },
                "wait_for_selector": { "type": ["string", "null"], "minLength": 1 },
                "project": { "type": ["string", "null"], "minLength": 1 },
                "viewport": { "type": ["string", "null"], "enum": ["desktop", "mobile", null] },
                "full_page": { "type": ["boolean", "null"] },
                "format": { "type": ["string", "null"], "enum": ["png", "jpeg", null] },
                "quality": { "type": ["integer", "null"], "minimum": 1, "maximum": 100 }
            }
        }),
    )
}

/// Validate that `raw` is an `http`/`https` URL whose host is a loopback
/// address, returning the parsed URL. This is the whole security posture of
/// this tool: it refuses to become a general browsing surface.
fn validate_loopback_url(raw: &str) -> Result<reqwest::Url, ToolError> {
    let parsed = reqwest::Url::parse(raw)
        .map_err(|e| ToolError::Validation(format!("ui_screenshot: invalid url `{raw}`: {e}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ToolError::Validation(format!(
            "ui_screenshot: unsupported scheme `{}` in `{raw}` — only http/https loopback URLs \
             are accepted.",
            parsed.scheme()
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::Validation(format!("ui_screenshot: `{raw}` has no host")))?;
    // `Url::host_str()` wraps an IPv6 literal in brackets (`"[::1]"`); strip
    // them before parsing as an `IpAddr` so `::1` is still recognised as
    // loopback.
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    let is_loopback = bare_host.eq_ignore_ascii_case("localhost")
        || bare_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !is_loopback {
        return Err(ToolError::Validation(format!(
            "ui_screenshot: `{host}` is not a loopback address. This tool only screenshots the \
             app YOU are building, reachable at 127.0.0.1 / ::1 / localhost inside this \
             container — it is not a general browsing capability. For a real, external site use \
             `browser_render` instead."
        )));
    }
    Ok(parsed)
}

/// The clean, actionable error the minimal image profile hits: chromium is
/// simply not installed there. Never a crash — this is the whole point of
/// probing at call time (see the module docs).
fn chromium_missing_error() -> ToolError {
    ToolError::Validation(
        "ui_screenshot: chromium is not installed in this container. It ships with the \
         `prototyping` image profile — switch this group to it and restart, e.g. \
         `cclaw groups config update --field 'image_profile=\"prototyping\"' <group-id>` then \
         `cclaw groups restart <group-id>`. Until then there is no in-container browser to take \
         a screenshot with; `diagnostics`/`shell` still work without it."
            .to_string(),
    )
}

/// Resolve where the PNG is saved: `<project>/.copperclaw/screenshots/`.
/// `project` mirrors `shell`'s `cwd` argument — a path the model already
/// knows (e.g. the one it scaffolded the app under). Absolute paths under
/// the data root resolve via [`crate::tools::verify_gate::project_root_of`]
/// (so an as-yet-uncreated project directory still resolves, matching the
/// edit-tools' dirty-marking convention); anything else falls back to
/// joining it under the data root. Omitted → the data root's own
/// `.copperclaw/screenshots/`.
fn resolve_screenshot_dir(project: Option<&str>) -> PathBuf {
    let root = match project {
        None => crate::tools::verify_gate::data_root(),
        Some(p) => crate::tools::verify_gate::project_root_of(p).unwrap_or_else(|| {
            let path = Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                crate::tools::verify_gate::data_root().join(path)
            }
        }),
    };
    root.join(".copperclaw").join("screenshots")
}

/// Resolve the screenshot-mode [`CaptureOptions`] from `input`. With NONE of
/// `viewport`/`full_page`/`format`/`quality` set, this is byte-identical to
/// [`CaptureOptions::ui_screenshot_default`] — D1's fixed windowed 1280x800
/// PNG.
fn resolve_capture_options(input: &Input) -> Result<CaptureOptions, ToolError> {
    let viewport = match input.viewport.as_deref() {
        Some(s) => ViewportPreset::parse(s)
            .map_err(|e| ToolError::Validation(format!("ui_screenshot: {e}")))?,
        None => ViewportPreset::Desktop,
    };
    let format = match input.format.as_deref() {
        Some(s) => ImageFormat::parse(s)
            .map_err(|e| ToolError::Validation(format!("ui_screenshot: {e}")))?,
        None => ImageFormat::Png,
    };
    Ok(CaptureOptions {
        viewport: Some(viewport),
        full_page: input.full_page.unwrap_or(false),
        format,
        quality: input.quality.map(|q| q.clamp(1, 100)),
    })
}

/// Everything up to (but not including) driving a real chromium: parse,
/// loopback-validate, and resolve the save directory / capture knobs. Split
/// out so the validation/resolution logic is unit-tested without touching a
/// real chromium binary.
struct Prepared {
    url: reqwest::Url,
    screenshot_dir: PathBuf,
    wait_ms: Option<u64>,
    wait_for_selector: Option<String>,
    capture: CaptureOptions,
}

fn prepare(input: &Input) -> Result<Prepared, ToolError> {
    let url = validate_loopback_url(&input.url)?;
    let screenshot_dir = resolve_screenshot_dir(input.project.as_deref());
    let capture = resolve_capture_options(input)?;
    Ok(Prepared {
        url,
        screenshot_dir,
        wait_ms: input.wait_ms,
        wait_for_selector: input.wait_for_selector.clone(),
        capture,
    })
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let prepared = prepare(&input)?;

    // Probe for chromium AT CALL TIME (not registration time — the tool is
    // always registered under Coding/Full; the minimal profile degrades
    // here, cleanly).
    let binary = copperclaw_browser::find_chromium_binary().ok_or_else(chromium_missing_error)?;

    let transport = copperclaw_browser::chromium_singleton()
        .get_transport(&binary, NAV_TIMEOUT)
        .await
        .map_err(|e| {
            ToolError::Internal(format!(
                "ui_screenshot: could not start the local chromium: {e}"
            ))
        })?;

    let req = copperclaw_browser::ScreenshotRequest {
        url: prepared.url.to_string(),
        capture: prepared.capture.clone(),
        wait_ms: prepared.wait_ms,
        wait_for_selector: prepared.wait_for_selector.clone(),
        nav_timeout: NAV_TIMEOUT,
    };
    // M20 D2 size safety: an oversize capture auto-retries once as a
    // lower-fidelity jpeg rather than erroring outright.
    let result = copperclaw_browser::capture_with_size_safety(
        transport.as_ref(),
        &req,
        MAX_SCREENSHOT_BYTES,
    )
    .await
    .map_err(|e| ToolError::Internal(format!("ui_screenshot: {e}")))?;

    if result.bytes.len() as u64 > MAX_SCREENSHOT_BYTES {
        return Err(ToolError::Internal(format!(
            "ui_screenshot: the capture of `{}` was {} bytes, over the {MAX_SCREENSHOT_BYTES}-byte \
             cap even after an automatic retry at a lower-fidelity jpeg — unusual; try again after \
             the page settles (e.g. via `wait_for_selector`), or pass `full_page: false` / a \
             smaller viewport.",
            prepared.url,
            result.bytes.len()
        )));
    }

    tokio::fs::create_dir_all(&prepared.screenshot_dir)
        .await
        .map_err(|e| {
            ToolError::Internal(format!(
                "ui_screenshot: create screenshot dir `{}`: {e}",
                prepared.screenshot_dir.display()
            ))
        })?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ext = result.capture.format.file_extension();
    let path = prepared.screenshot_dir.join(format!("ui-{nanos}.{ext}"));
    tokio::fs::write(&path, &result.bytes).await.map_err(|e| {
        ToolError::Internal(format!("ui_screenshot: write `{}`: {e}", path.display()))
    })?;

    let (width, height) = result.capture.viewport.map_or(
        (
            ViewportPreset::DESKTOP_WIDTH,
            ViewportPreset::DESKTOP_HEIGHT,
        ),
        ViewportPreset::dimensions,
    );
    let downgrade_note = if result.downgraded {
        format!(
            " (note: auto-downgraded to jpeg q={} because the original {} capture exceeded the \
             {MAX_SCREENSHOT_BYTES}-byte cap)",
            copperclaw_browser::DOWNGRADE_JPEG_QUALITY,
            prepared.capture.format.as_cdp_str(),
        )
    } else {
        String::new()
    };

    let b64 = bytes_b64::encode(&result.bytes);
    Ok(CallToolResult::success(vec![
        Content::text(format!(
            "Captured a {width}x{height} {} screenshot of {} ({} bytes){downgrade_note}; saved to \
             {} (use `send_file` to share it) — it is attached below for you to see.",
            result.capture.format.as_cdp_str(),
            prepared.url,
            result.bytes.len(),
            path.display(),
        )),
        Content::image(b64, result.capture.format.mime_type().to_string()),
    ]))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── loopback URL validation ──────────────────────────────────────────

    #[test]
    fn accepts_127_0_0_1() {
        assert!(validate_loopback_url("http://127.0.0.1:5173/").is_ok());
    }

    #[test]
    fn accepts_localhost_case_insensitive() {
        assert!(validate_loopback_url("http://LocalHost:3000/app").is_ok());
    }

    #[test]
    fn accepts_ipv6_loopback() {
        assert!(validate_loopback_url("http://[::1]:8080/").is_ok());
    }

    #[test]
    fn accepts_https_loopback() {
        assert!(validate_loopback_url("https://127.0.0.1:8443/").is_ok());
    }

    #[test]
    fn refuses_external_host() {
        let err = validate_loopback_url("http://example.com/").unwrap_err();
        match err {
            ToolError::Validation(m) => {
                assert!(m.contains("loopback"), "{m}");
                assert!(m.contains("browser_render"), "{m}");
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn refuses_rfc1918_address() {
        assert!(validate_loopback_url("http://10.0.0.5/").is_err());
    }

    #[test]
    fn refuses_link_local_metadata_address() {
        // Not loopback — the classic SSRF pivot target, refused for the same
        // reason any other non-loopback host is.
        assert!(validate_loopback_url("http://169.254.169.254/").is_err());
    }

    #[test]
    fn refuses_non_http_scheme() {
        let err = validate_loopback_url("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn refuses_malformed_url() {
        assert!(validate_loopback_url("not a url").is_err());
    }

    // ── missing-chromium actionable error ────────────────────────────────

    #[test]
    fn chromium_missing_error_names_prototyping_profile() {
        let err = chromium_missing_error();
        match err {
            ToolError::Validation(m) => {
                assert!(m.contains("chromium"), "{m}");
                assert!(m.contains("prototyping"), "{m}");
                assert!(m.contains("image_profile"), "{m}");
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    // ── screenshot-dir resolution (`.copperclaw/screenshots/`) ───────────

    /// Guards the shared `verify_gate` data-root override for this test
    /// module, mirroring `verify_gate.rs`'s own `DataRootGuard` (private to
    /// that module) so these tests never touch the real `/data` root.
    struct DataRootGuard {
        dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl DataRootGuard {
        fn new() -> Self {
            let lock = crate::tools::verify_gate::data_root_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            crate::tools::verify_gate::data_root_test_override_set(dir.path().to_path_buf());
            Self { dir, _lock: lock }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    impl Drop for DataRootGuard {
        fn drop(&mut self) {
            crate::tools::verify_gate::data_root_test_override_clear();
        }
    }

    #[test]
    fn resolves_under_data_root_when_project_omitted() {
        let g = DataRootGuard::new();
        let dir = resolve_screenshot_dir(None);
        assert_eq!(dir, g.path().join(".copperclaw").join("screenshots"));
    }

    #[test]
    fn resolves_under_named_project_when_given() {
        let g = DataRootGuard::new();
        let proj = g.path().join("myapp");
        std::fs::create_dir_all(&proj).unwrap();
        let dir = resolve_screenshot_dir(Some(proj.to_str().unwrap()));
        assert_eq!(dir, proj.join(".copperclaw").join("screenshots"));
    }

    #[test]
    fn resolves_under_not_yet_created_project() {
        // A project scaffolded moments ago (dir not created yet) still
        // resolves, matching the edit-tools' `project_root_of` convention.
        let g = DataRootGuard::new();
        let proj = g.path().join("brand-new-app");
        let dir = resolve_screenshot_dir(Some(proj.to_str().unwrap()));
        assert_eq!(dir, proj.join(".copperclaw").join("screenshots"));
    }

    #[test]
    fn resolves_relative_project_name_under_data_root() {
        let g = DataRootGuard::new();
        let dir = resolve_screenshot_dir(Some("myapp"));
        assert_eq!(
            dir,
            g.path()
                .join("myapp")
                .join(".copperclaw")
                .join("screenshots")
        );
    }

    // ── schema ────────────────────────────────────────────────────────────

    #[test]
    fn schema_advertises_loopback_only_and_prototyping_requirement() {
        let tool = schema();
        assert_eq!(tool.name, "ui_screenshot");
        let desc = tool
            .description
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(desc.contains("loopback"), "{desc}");
        assert!(desc.contains("browser_render"), "{desc}");
        assert!(desc.contains("prototyping"), "{desc}");
    }

    #[test]
    fn prepare_rejects_non_loopback_before_touching_chromium() {
        let input = Input {
            url: "http://example.com/".to_string(),
            wait_ms: None,
            wait_for_selector: None,
            project: None,
            viewport: None,
            full_page: None,
            format: None,
            quality: None,
        };
        assert!(prepare(&input).is_err());
    }

    // ── M20 D2: capture fidelity (viewport / format / full-page) ─────────

    fn bare_input(url: &str) -> Input {
        Input {
            url: url.to_string(),
            wait_ms: None,
            wait_for_selector: None,
            project: None,
            viewport: None,
            full_page: None,
            format: None,
            quality: None,
        }
    }

    #[test]
    fn resolve_capture_options_with_no_args_is_byte_identical_to_d1_default() {
        // THE critical D1 back-compat property: no viewport/format/full_page
        // args gets the EXACT pre-D2 capture options (1280x800 windowed png).
        let opts = resolve_capture_options(&bare_input("http://127.0.0.1:5173/")).unwrap();
        assert_eq!(opts, CaptureOptions::ui_screenshot_default());
    }

    #[test]
    fn resolve_capture_options_mobile_preset() {
        let mut i = bare_input("http://127.0.0.1:5173/");
        i.viewport = Some("mobile".to_string());
        let opts = resolve_capture_options(&i).unwrap();
        assert_eq!(opts.viewport, Some(ViewportPreset::Mobile));
        // full_page stays the ui_screenshot default (false) unless overridden.
        assert!(!opts.full_page);
    }

    #[test]
    fn resolve_capture_options_full_page_and_format_overrides() {
        let mut i = bare_input("http://127.0.0.1:5173/");
        i.full_page = Some(true);
        i.format = Some("jpeg".to_string());
        i.quality = Some(85);
        let opts = resolve_capture_options(&i).unwrap();
        assert!(opts.full_page);
        assert_eq!(opts.format, ImageFormat::Jpeg);
        assert_eq!(opts.quality, Some(85));
        // Viewport still defaults to desktop even with other overrides set.
        assert_eq!(opts.viewport, Some(ViewportPreset::Desktop));
    }

    #[test]
    fn resolve_capture_options_rejects_unknown_viewport() {
        let mut i = bare_input("http://127.0.0.1:5173/");
        i.viewport = Some("tablet".to_string());
        assert!(matches!(
            resolve_capture_options(&i),
            Err(ToolError::Validation(_))
        ));
    }

    #[test]
    fn resolve_capture_options_rejects_unknown_format() {
        let mut i = bare_input("http://127.0.0.1:5173/");
        i.format = Some("bmp".to_string());
        assert!(matches!(
            resolve_capture_options(&i),
            Err(ToolError::Validation(_))
        ));
    }

    #[test]
    fn resolve_capture_options_clamps_quality_to_valid_range() {
        let mut i = bare_input("http://127.0.0.1:5173/");
        i.quality = Some(255);
        let opts = resolve_capture_options(&i).unwrap();
        assert_eq!(opts.quality, Some(100));
    }

    #[test]
    fn prepare_with_no_args_yields_d1_default_capture() {
        let prepared = prepare(&bare_input("http://127.0.0.1:5173/")).unwrap();
        assert_eq!(prepared.capture, CaptureOptions::ui_screenshot_default());
    }

    #[test]
    fn schema_advertises_capture_fidelity_args() {
        let tool = schema();
        let v: serde_json::Value = serde_json::to_value(&*tool.input_schema).unwrap();
        let props = v.get("properties").unwrap();
        for key in ["viewport", "full_page", "format", "quality"] {
            assert!(props.get(key).is_some(), "schema missing `{key}`");
        }
    }

    // ── live in-container acceptance (Docker/prototyping-image-gated) ────

    /// Live in-container acceptance: start a vite dev server, then
    /// `ui_screenshot("http://127.0.0.1:5173")` should return an image block
    /// the provider path converts, save the PNG under
    /// `.copperclaw/screenshots/`, and stay under the 5 MB cap at the
    /// default viewport. Needs the prototyping image's baked chromium AND
    /// a real dev server — `#[ignore]`d per the `session_install_docker_end_to_end`
    /// precedent (`crate::tools::self_mod`). Opt in with
    /// `cargo test -p copperclaw-mcp -- --ignored ui_screenshot_docker_end_to_end`.
    #[tokio::test]
    #[ignore = "requires the prototyping image's chromium + a running dev server on 127.0.0.1; opt in with --ignored"]
    async fn ui_screenshot_docker_end_to_end() {
        let mut args = JsonObject::new();
        args.insert("url".into(), "http://127.0.0.1:5173/".into());
        let ctx = crate::context::MockToolContext::new();
        let res = handle(Some(args), &ctx)
            .await
            .expect("ui_screenshot should succeed against a live dev server");
        assert_eq!(res.is_error, Some(false));
        let has_image = res
            .content
            .iter()
            .any(|c| matches!(c.raw, rmcp::model::RawContent::Image(_)));
        assert!(has_image, "expected an image content block");
    }

    /// M20 D2 live smoke: a `viewport: "mobile"` capture reports different
    /// dimensions than the `desktop` default against the SAME vite page —
    /// proving the preset actually changes what gets captured, not just the
    /// advertised default. Same gating as the D1 acceptance test above.
    #[tokio::test]
    #[ignore = "requires the prototyping image's chromium + a running dev server on 127.0.0.1; opt in with --ignored"]
    async fn ui_screenshot_mobile_preset_differs_in_dimensions_from_desktop() {
        let ctx = crate::context::MockToolContext::new();

        let mut desktop_args = JsonObject::new();
        desktop_args.insert("url".into(), "http://127.0.0.1:5173/".into());
        let desktop_res = handle(Some(desktop_args), &ctx)
            .await
            .expect("desktop-default capture should succeed");
        let desktop_text = desktop_res
            .content
            .iter()
            .find_map(|c| match &c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .expect("a text part");
        assert!(
            desktop_text.contains("1280x800"),
            "desktop default should report 1280x800: {desktop_text}"
        );

        let mut mobile_args = JsonObject::new();
        mobile_args.insert("url".into(), "http://127.0.0.1:5173/".into());
        mobile_args.insert("viewport".into(), "mobile".into());
        let mobile_res = handle(Some(mobile_args), &ctx)
            .await
            .expect("mobile-preset capture should succeed");
        let mobile_text = mobile_res
            .content
            .iter()
            .find_map(|c| match &c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .expect("a text part");
        assert!(
            mobile_text.contains("390x844"),
            "mobile preset should report 390x844: {mobile_text}"
        );
        assert_ne!(
            desktop_text, mobile_text,
            "mobile and desktop captures of the same page must differ"
        );
    }
}
