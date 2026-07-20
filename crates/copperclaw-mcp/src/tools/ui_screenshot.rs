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
//!   * **Conditionally tainted (M20 D5).** Unlike `browser_render` (a general
//!     web-browsing tool over attacker-influenceable content), the page this
//!     tool screenshots is the agent's OWN in-progress build, reachable only
//!     on loopback. The IMAGE block itself follows the same path as
//!     `view_image` (which does not taint) — a screenshot with a clean
//!     console adds no page-derived TEXT to the transcript, only a trusted,
//!     host-generated save path. D5 folds a console-error count + the first
//!     error's TEXT into the response so the common case needs no second
//!     `ui_inspect` call — that text IS page-originated (the page's own
//!     console could echo fetched/attacker-influenced content), so whenever
//!     it is non-empty this handler calls `mark_untrusted_context` before
//!     returning. Full console detail always lives behind `ui_inspect`,
//!     which taints unconditionally (see its module docs).
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
         downgrade — it never just errors. The text response also reports a console error/warning \
         count (plus the first error's text) when the page logged any during this navigation — \
         call `ui_inspect` for full console detail and element geometry. Each capture is also \
         diffed against the previous screenshot of the same view (url + viewport): if the layout \
         shifted, a short visual-regression note is appended so you can catch a UI you edited \
         silently breaking — clean re-captures stay quiet. Requires chromium, which \
         is baked into the `prototyping` image profile; on a minimal-profile container this \
         returns a clear, actionable error instead of failing silently.",
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
/// this tool: it refuses to become a general browsing surface. `pub(crate)`
/// so `ui_inspect` (M20 D5) reuses the exact same check rather than
/// duplicating it.
pub(crate) fn validate_loopback_url(raw: &str) -> Result<reqwest::Url, ToolError> {
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
/// probing at call time (see the module docs). `pub(crate)` so `ui_inspect`
/// (M20 D5) reuses the exact same message.
pub(crate) fn chromium_missing_error() -> ToolError {
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

/// Format the optional console-error fold-in note appended to `ui_screenshot`'s
/// text response (M20 D5) — the common case (a page that threw on load, or
/// logged an error) needs no second `ui_inspect` call to at least learn a
/// crash happened. Empty when there were no console errors. Pure so this is
/// unit-tested without a live transport.
fn console_fold_in_note(summary: &copperclaw_browser::ConsoleSummary) -> String {
    if summary.error_count == 0 {
        return String::new();
    }
    let warn_part = if summary.warning_count > 0 {
        format!(", {} warning(s)", summary.warning_count)
    } else {
        String::new()
    };
    let first = summary
        .first_error
        .as_deref()
        .map(|e| format!(": {e}"))
        .unwrap_or_default();
    format!(
        " Console: {} error(s){warn_part}{first} — call `ui_inspect` for full detail.",
        summary.error_count
    )
}

/// Env flag that opts out of the C4 screenshot-diff visual-regression check.
/// Default ON; `0`/`false`/`off`/`no` (case-insensitive, trimmed) turns it
/// off — mirroring `diagnostics`' [`COPPERCLAW_POST_EDIT_VERIFY`] opt-out so
/// the two see→fix feedback hooks share one mental model.
const UI_DIFF_ENV: &str = "COPPERCLAW_UI_SCREENSHOT_DIFF";

/// Interpret the [`UI_DIFF_ENV`] value. Pure so it's unit-tested without
/// mutating the process environment (forbidden under the workspace's
/// `unsafe`-free rules).
fn parse_ui_diff_flag(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
    }
}

/// Whether the visual-regression diff runs this call. Default ON; reads
/// [`UI_DIFF_ENV`] lazily at call time (mirroring how the rest of this tool
/// probes the environment).
fn ui_diff_enabled() -> bool {
    parse_ui_diff_flag(std::env::var(UI_DIFF_ENV).ok().as_deref())
}

/// C4 — screenshot-diff visual regression.
///
/// Editing an existing UI can silently regress its layout, and nothing before
/// this compared the *before* and *after* of a `ui_screenshot`. This module
/// keeps a per-`(url, viewport, full_page)` PNG **baseline** under the
/// project's `.copperclaw/baselines/` and, on the next capture of the same
/// view, computes a perceptual **block diff** against it — surfacing a concise
/// digest appended to the tool result (the same see→fix feedback shape C1's
/// `diagnostics::append_post_edit_digest` uses for type/format breakage). The
/// current capture then *becomes* the new baseline, so consecutive shots of a
/// page you are iterating on are diffed pairwise.
///
/// It is deliberately self-contained: chromium emits PNG and the workspace
/// carries no image-decoding crate, so this ships a small, safe (`unsafe`-free)
/// PNG decoder — zlib/inflate + per-scanline unfilter, the exact non-interlaced
/// 8-bit subset chromium and the fixtures use — plus a threshold-based block
/// comparison. Everything is best-effort: any decode/IO hiccup skips the diff
/// silently and never turns a good screenshot into an error (C1's discipline).
///
/// Loopback-only and reuses the existing screenshot path — no new capability
/// (no security review; see the module docs).
mod visual {
    // A self-contained image codec + pixel-math module: intentional narrowing
    // casts (bytes ↔ indices ↔ channel sums) and short mathematical names
    // (Paeth `a`/`b`/`c`, per-channel `dr`/`dg`/`db`) are clearer than the
    // `from`/`try_from` churn pedantic would otherwise demand here.
    #![allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_lossless,
        clippy::cast_possible_wrap,
        clippy::similar_names,
        clippy::many_single_char_names
    )]

    use std::path::{Path, PathBuf};

    /// Block edge (pixels). The view is tiled into `BLOCK`×`BLOCK` cells and a
    /// cell that changed "enough" counts as one changed region — coarse on
    /// purpose so a shifted card reads as a handful of regions, not thousands
    /// of stray pixels.
    const BLOCK: usize = 16;
    /// A pixel counts as *different* when the mean of its per-channel (R,G,B)
    /// absolute deltas exceeds this — high enough to shrug off JPEG/anti-alias
    /// noise, low enough to catch a real colour/content change.
    const PIXEL_DELTA_THRESHOLD: u16 = 16;
    /// A block counts as *changed* when at least this fraction of its pixels
    /// are different — one stray pixel never flags a region.
    const BLOCK_CHANGED_FRACTION: f64 = 0.02;

    /// A decoded, always-RGBA8 image (4 bytes/pixel, row-major).
    pub(super) struct DecodedImage {
        pub width: u32,
        pub height: u32,
        pub rgba: Vec<u8>,
    }

    /// The perceptual-diff result surfaced back to the model.
    #[derive(Debug, Default)]
    pub(super) struct DiffDigest {
        pub flagged: bool,
        pub dimensions_changed: bool,
        pub base_dims: (u32, u32),
        pub new_dims: (u32, u32),
        pub changed_regions: usize,
        pub total_regions: usize,
        pub changed_fraction: f64,
        pub mean_delta: f64,
        pub max_block_delta: f64,
        /// Bounding box (x0,y0,x1,y1) in pixels enclosing all changed blocks.
        pub bbox: Option<(u32, u32, u32, u32)>,
    }

    // ── bit reader (DEFLATE is LSB-first) ────────────────────────────────

    struct BitReader<'a> {
        data: &'a [u8],
        pos: usize,
        bit: u32,
    }

    impl<'a> BitReader<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self {
                data,
                pos: 0,
                bit: 0,
            }
        }

        fn read_bit(&mut self) -> Result<u32, String> {
            let byte = *self
                .data
                .get(self.pos)
                .ok_or("inflate: unexpected end of stream")?;
            let b = u32::from((byte >> self.bit) & 1);
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
            Ok(b)
        }

        fn read_bits(&mut self, n: u32) -> Result<u32, String> {
            let mut v = 0u32;
            for i in 0..n {
                v |= self.read_bit()? << i;
            }
            Ok(v)
        }

        fn align_byte(&mut self) {
            if self.bit != 0 {
                self.bit = 0;
                self.pos += 1;
            }
        }

        fn read_byte_aligned(&mut self) -> Result<u8, String> {
            let byte = *self
                .data
                .get(self.pos)
                .ok_or("inflate: unexpected end of stored block")?;
            self.pos += 1;
            Ok(byte)
        }
    }

    // ── canonical Huffman (puff.c algorithm, ported to safe Rust) ────────

    const MAXBITS: usize = 15;

    struct Huffman {
        count: [u16; MAXBITS + 1],
        symbol: Vec<u16>,
    }

    fn build_huffman(lengths: &[u8]) -> Huffman {
        let mut count = [0u16; MAXBITS + 1];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut offs = [0u16; MAXBITS + 2];
        for len in 1..=MAXBITS {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Huffman { count, symbol }
    }

    fn decode_symbol(r: &mut BitReader, h: &Huffman) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAXBITS {
            code |= r.read_bit()? as i32;
            let cnt = i32::from(h.count[len]);
            if code - first < cnt {
                return h
                    .symbol
                    .get((index + (code - first)) as usize)
                    .copied()
                    .ok_or_else(|| "inflate: symbol index out of range".to_string());
            }
            index += cnt;
            first = (first + cnt) << 1;
            code <<= 1;
        }
        Err("inflate: invalid huffman code".into())
    }

    // ── inflate ──────────────────────────────────────────────────────────

    #[rustfmt::skip]
    const LEN_BASE: [u16; 29] = [
        3,4,5,6,7,8,9,10,11,13,15,17,19,23,27,31,35,43,51,59,67,83,99,115,131,163,195,227,258,
    ];
    #[rustfmt::skip]
    const LEN_EXTRA: [u8; 29] = [
        0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,0,
    ];
    #[rustfmt::skip]
    const DIST_BASE: [u16; 30] = [
        1,2,3,4,5,7,9,13,17,25,33,49,65,97,129,193,257,385,513,769,
        1025,1537,2049,3073,4097,6145,8193,12289,16385,24577,
    ];
    #[rustfmt::skip]
    const DIST_EXTRA: [u8; 30] = [
        0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13,
    ];

    fn fixed_tables() -> (Huffman, Huffman) {
        // Fixed lit/len code lengths: 8 for 0-143 and 280-287, 9 for 144-255,
        // 7 for 256-279 (RFC 1951 §3.2.6); default 8 covers both 8-bit ranges.
        let mut lit = [8u8; 288];
        for (i, l) in lit.iter_mut().enumerate() {
            if (144..=255).contains(&i) {
                *l = 9;
            } else if (256..=279).contains(&i) {
                *l = 7;
            }
        }
        (build_huffman(&lit), build_huffman(&[5u8; 30]))
    }

    fn dynamic_tables(r: &mut BitReader) -> Result<(Huffman, Huffman), String> {
        const ORDER: [usize; 19] = [
            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
        ];
        let hlit = r.read_bits(5)? as usize + 257;
        let hdist = r.read_bits(5)? as usize + 1;
        let hclen = r.read_bits(4)? as usize + 4;
        if hlit > 286 || hdist > 30 || hclen > 19 {
            return Err("inflate: dynamic-table size out of range".into());
        }
        let mut cl = [0u8; 19];
        for &slot in ORDER.iter().take(hclen) {
            cl[slot] = r.read_bits(3)? as u8;
        }
        let cl_huff = build_huffman(&cl);
        let total = hlit + hdist;
        let mut lengths = vec![0u8; total];
        let mut i = 0;
        while i < total {
            let sym = decode_symbol(r, &cl_huff)?;
            match sym {
                0..=15 => {
                    lengths[i] = sym as u8;
                    i += 1;
                }
                16 => {
                    if i == 0 {
                        return Err("inflate: repeat with no previous length".into());
                    }
                    let prev = lengths[i - 1];
                    let rep = 3 + r.read_bits(2)? as usize;
                    for _ in 0..rep {
                        if i >= total {
                            return Err("inflate: code-length overrun".into());
                        }
                        lengths[i] = prev;
                        i += 1;
                    }
                }
                17 => {
                    let rep = 3 + r.read_bits(3)? as usize;
                    i = fill_zeros(&mut lengths, i, rep)?;
                }
                18 => {
                    let rep = 11 + r.read_bits(7)? as usize;
                    i = fill_zeros(&mut lengths, i, rep)?;
                }
                _ => return Err("inflate: invalid code-length symbol".into()),
            }
        }
        let lit = build_huffman(&lengths[..hlit]);
        let dist = build_huffman(&lengths[hlit..]);
        Ok((lit, dist))
    }

    fn fill_zeros(lengths: &mut [u8], mut i: usize, rep: usize) -> Result<usize, String> {
        for _ in 0..rep {
            if i >= lengths.len() {
                return Err("inflate: code-length overrun".into());
            }
            lengths[i] = 0;
            i += 1;
        }
        Ok(i)
    }

    fn inflate_block(
        r: &mut BitReader,
        out: &mut Vec<u8>,
        lit: &Huffman,
        dist: &Huffman,
        cap: usize,
    ) -> Result<(), String> {
        loop {
            let sym = decode_symbol(r, lit)?;
            match sym {
                256 => return Ok(()),
                0..=255 => out.push(sym as u8),
                257..=285 => {
                    let s = (sym - 257) as usize;
                    let len = LEN_BASE[s] as usize + r.read_bits(u32::from(LEN_EXTRA[s]))? as usize;
                    let dsym = decode_symbol(r, dist)? as usize;
                    if dsym >= 30 {
                        return Err("inflate: invalid distance symbol".into());
                    }
                    let distance = DIST_BASE[dsym] as usize
                        + r.read_bits(u32::from(DIST_EXTRA[dsym]))? as usize;
                    if distance == 0 || distance > out.len() {
                        return Err("inflate: bad back-reference".into());
                    }
                    let start = out.len() - distance;
                    for k in 0..len {
                        let b = out[start + k];
                        out.push(b);
                    }
                }
                _ => return Err("inflate: invalid length symbol".into()),
            }
            if out.len() > cap {
                return Err("inflate: output exceeds expected size".into());
            }
        }
    }

    fn inflate(data: &[u8], cap: usize) -> Result<Vec<u8>, String> {
        let mut r = BitReader::new(data);
        let mut out = Vec::with_capacity(cap);
        loop {
            let bfinal = r.read_bit()?;
            match r.read_bits(2)? {
                0 => {
                    r.align_byte();
                    let len = u16::from(r.read_byte_aligned()?)
                        | (u16::from(r.read_byte_aligned()?) << 8);
                    let nlen = u16::from(r.read_byte_aligned()?)
                        | (u16::from(r.read_byte_aligned()?) << 8);
                    if len != !nlen {
                        return Err("inflate: stored-block length check failed".into());
                    }
                    for _ in 0..len {
                        out.push(r.read_byte_aligned()?);
                        if out.len() > cap {
                            return Err("inflate: output exceeds expected size".into());
                        }
                    }
                }
                1 => {
                    let (lit, dist) = fixed_tables();
                    inflate_block(&mut r, &mut out, &lit, &dist, cap)?;
                }
                2 => {
                    let (lit, dist) = dynamic_tables(&mut r)?;
                    inflate_block(&mut r, &mut out, &lit, &dist, cap)?;
                }
                _ => return Err("inflate: invalid block type".into()),
            }
            if bfinal == 1 {
                return Ok(out);
            }
        }
    }

    /// Strip the 2-byte zlib wrapper (and optional preset-dictionary id) and
    /// inflate the DEFLATE stream to at most `cap` bytes.
    fn inflate_zlib(data: &[u8], cap: usize) -> Result<Vec<u8>, String> {
        if data.len() < 2 {
            return Err("zlib: stream too short".into());
        }
        let (cmf, flg) = (data[0], data[1]);
        if cmf & 0x0f != 8 {
            return Err("zlib: unsupported compression method".into());
        }
        if (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
            return Err("zlib: header check failed".into());
        }
        if flg & 0x20 != 0 {
            return Err("zlib: preset dictionary unsupported".into());
        }
        inflate(&data[2..], cap)
    }

    // ── PNG decode ───────────────────────────────────────────────────────

    const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    /// Upper bound on decoded pixels — guards against a malformed IHDR asking
    /// for gigabytes. 64 MP dwarfs any real screenshot.
    const MAX_PIXELS: usize = 64_000_000;

    /// Decode a non-interlaced, 8-bit PNG (grayscale / GA / RGB / RGBA — the
    /// subset chromium and the fixtures emit) into RGBA8. Palette (color type
    /// 3) and non-8-bit depths are rejected: the diff simply skips such a
    /// capture rather than guessing.
    pub(super) fn decode_png(bytes: &[u8]) -> Result<DecodedImage, String> {
        if bytes.len() < 8 || bytes[..8] != PNG_SIG {
            return Err("png: bad signature".into());
        }
        let mut pos = 8;
        let (mut width, mut height) = (0u32, 0u32);
        let (mut bit_depth, mut color_type, mut interlace) = (0u8, 0u8, 0u8);
        let mut seen_ihdr = false;
        let mut idat: Vec<u8> = Vec::new();
        while pos + 8 <= bytes.len() {
            let len =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            let ctype = &bytes[pos + 4..pos + 8];
            let data_start = pos + 8;
            let data_end = data_start
                .checked_add(len)
                .ok_or("png: chunk length overflow")?;
            if data_end + 4 > bytes.len() {
                return Err("png: truncated chunk".into());
            }
            match ctype {
                b"IHDR" => {
                    if len < 13 {
                        return Err("png: short IHDR".into());
                    }
                    let c = &bytes[data_start..data_start + 13];
                    width = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                    height = u32::from_be_bytes([c[4], c[5], c[6], c[7]]);
                    bit_depth = c[8];
                    color_type = c[9];
                    interlace = c[12];
                    seen_ihdr = true;
                }
                b"IDAT" => idat.extend_from_slice(&bytes[data_start..data_end]),
                b"IEND" => break,
                _ => {}
            }
            pos = data_end + 4; // + 4-byte CRC (unverified — best-effort read path)
        }
        if !seen_ihdr {
            return Err("png: missing IHDR".into());
        }
        if bit_depth != 8 {
            return Err(format!("png: unsupported bit depth {bit_depth}"));
        }
        if interlace != 0 {
            return Err("png: interlaced images unsupported".into());
        }
        let channels: usize = match color_type {
            0 => 1,
            2 => 3,
            4 => 2,
            6 => 4,
            other => return Err(format!("png: unsupported color type {other}")),
        };
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 {
            return Err("png: zero dimension".into());
        }
        if w.saturating_mul(h) > MAX_PIXELS {
            return Err("png: image exceeds pixel cap".into());
        }
        let stride = w * channels;
        let expected = h * (stride + 1); // one filter-type byte per scanline
        let raw = inflate_zlib(&idat, expected)?;
        if raw.len() < expected {
            return Err("png: inflated data shorter than IHDR implies".into());
        }
        let mut prev = vec![0u8; stride];
        let mut rgba = vec![0u8; w * h * 4];
        let mut src = 0usize;
        for y in 0..h {
            let filter = raw[src];
            src += 1;
            let mut line = raw[src..src + stride].to_vec();
            src += stride;
            unfilter(filter, &mut line, &prev, channels)?;
            for x in 0..w {
                let base = x * channels;
                let (r, g, b, a) = match channels {
                    1 => (line[base], line[base], line[base], 255),
                    2 => (line[base], line[base], line[base], line[base + 1]),
                    3 => (line[base], line[base + 1], line[base + 2], 255),
                    _ => (line[base], line[base + 1], line[base + 2], line[base + 3]),
                };
                let o = (y * w + x) * 4;
                rgba[o] = r;
                rgba[o + 1] = g;
                rgba[o + 2] = b;
                rgba[o + 3] = a;
            }
            prev = line;
        }
        Ok(DecodedImage {
            width,
            height,
            rgba,
        })
    }

    /// Reverse one scanline's PNG filter in place (`bpp` = bytes per pixel).
    fn unfilter(filter: u8, line: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), String> {
        let n = line.len();
        match filter {
            0 => {}
            1 => {
                for i in bpp..n {
                    line[i] = line[i].wrapping_add(line[i - bpp]);
                }
            }
            2 => {
                for (l, p) in line.iter_mut().zip(prev.iter()) {
                    *l = l.wrapping_add(*p);
                }
            }
            3 => {
                for i in 0..n {
                    let a = if i >= bpp {
                        u16::from(line[i - bpp])
                    } else {
                        0
                    };
                    let b = u16::from(prev[i]);
                    line[i] = line[i].wrapping_add(((a + b) / 2) as u8);
                }
            }
            4 => {
                for i in 0..n {
                    let a = if i >= bpp {
                        i16::from(line[i - bpp])
                    } else {
                        0
                    };
                    let b = i16::from(prev[i]);
                    let c = if i >= bpp {
                        i16::from(prev[i - bpp])
                    } else {
                        0
                    };
                    line[i] = line[i].wrapping_add(paeth(a, b, c));
                }
            }
            other => return Err(format!("png: unknown filter type {other}")),
        }
        Ok(())
    }

    fn paeth(a: i16, b: i16, c: i16) -> u8 {
        let p = a + b - c;
        let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
        if pa <= pb && pa <= pc {
            a as u8
        } else if pb <= pc {
            b as u8
        } else {
            c as u8
        }
    }

    // ── perceptual block diff ────────────────────────────────────────────

    /// Compare two decoded images and summarise where they differ. A change in
    /// viewport dimensions is itself a layout regression, so it flags
    /// unconditionally; otherwise the overlapping area is tiled and blocks with
    /// enough differing pixels are counted as changed regions.
    pub(super) fn compute_diff(base: &DecodedImage, cur: &DecodedImage) -> DiffDigest {
        let base_dims = (base.width, base.height);
        let new_dims = (cur.width, cur.height);
        let dimensions_changed = base_dims != new_dims;

        let w = base.width.min(cur.width) as usize;
        let h = base.height.min(cur.height) as usize;
        let gw = w.div_ceil(BLOCK);
        let gh = h.div_ceil(BLOCK);
        let total_regions = gw * gh;

        let bw = base.width as usize;
        let cw = cur.width as usize;
        let mut changed_regions = 0usize;
        let mut max_block_delta = 0.0f64;
        let mut sum_delta = 0.0f64;
        let mut bbox: Option<(u32, u32, u32, u32)> = None;

        for by in 0..gh {
            for bx in 0..gw {
                let x0 = bx * BLOCK;
                let y0 = by * BLOCK;
                let x1 = (x0 + BLOCK).min(w);
                let y1 = (y0 + BLOCK).min(h);
                let mut changed_px = 0usize;
                let mut block_sum = 0.0f64;
                let mut px_count = 0usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let bi = (y * bw + x) * 4;
                        let ci = (y * cw + x) * 4;
                        let dr =
                            (i16::from(base.rgba[bi]) - i16::from(cur.rgba[ci])).unsigned_abs();
                        let dg = (i16::from(base.rgba[bi + 1]) - i16::from(cur.rgba[ci + 1]))
                            .unsigned_abs();
                        let db = (i16::from(base.rgba[bi + 2]) - i16::from(cur.rgba[ci + 2]))
                            .unsigned_abs();
                        let mean = (dr + dg + db) / 3;
                        block_sum += f64::from(mean);
                        if mean > PIXEL_DELTA_THRESHOLD {
                            changed_px += 1;
                        }
                        px_count += 1;
                    }
                }
                if px_count == 0 {
                    continue;
                }
                let block_mean = block_sum / px_count as f64;
                sum_delta += block_sum;
                if block_mean > max_block_delta {
                    max_block_delta = block_mean;
                }
                if changed_px as f64 / px_count as f64 >= BLOCK_CHANGED_FRACTION {
                    changed_regions += 1;
                    let (nx0, ny0, nx1, ny1) = (x0 as u32, y0 as u32, x1 as u32, y1 as u32);
                    bbox = Some(match bbox {
                        None => (nx0, ny0, nx1, ny1),
                        Some((ox0, oy0, ox1, oy1)) => {
                            (ox0.min(nx0), oy0.min(ny0), ox1.max(nx1), oy1.max(ny1))
                        }
                    });
                }
            }
        }

        let total_px = (w * h).max(1) as f64;
        let mean_delta = sum_delta / total_px;
        let changed_fraction = if total_regions == 0 {
            0.0
        } else {
            changed_regions as f64 / total_regions as f64
        };
        DiffDigest {
            flagged: dimensions_changed || changed_regions > 0,
            dimensions_changed,
            base_dims,
            new_dims,
            changed_regions,
            total_regions,
            changed_fraction,
            mean_delta,
            max_block_delta,
            bbox,
        }
    }

    /// The concise digest appended to `ui_screenshot`'s text response. Empty
    /// unless a regression is flagged — clean re-captures stay silent, exactly
    /// like C1's post-edit digest omits a clean edit.
    pub(super) fn diff_note(d: &DiffDigest) -> String {
        if !d.flagged {
            return String::new();
        }
        let dim_part = if d.dimensions_changed {
            format!(
                " The viewport dimensions changed ({}x{} → {}x{}), a strong layout-regression \
                 signal.",
                d.base_dims.0, d.base_dims.1, d.new_dims.0, d.new_dims.1
            )
        } else {
            String::new()
        };
        let bbox_part = d
            .bbox
            .map(|(x0, y0, x1, y1)| format!(", concentrated within x{x0}-{x1}, y{y0}-{y1}"))
            .unwrap_or_default();
        let opt_out = super::UI_DIFF_ENV;
        format!(
            " Visual regression vs the previous screenshot of this view: {} of {} region(s) \
             changed ({:.0}% of the view, mean pixel delta {:.1}/255, peak block delta \
             {:.1}/255{bbox_part}).{dim_part} If this shift was unintended, you may have \
             regressed the layout — re-check the affected area (opt out with {opt_out}=0).",
            d.changed_regions,
            d.total_regions,
            d.changed_fraction * 100.0,
            d.mean_delta,
            d.max_block_delta,
        )
    }

    /// FNV-1a of the view key → a stable baseline filename stem. Keeps one
    /// baseline per `(url, viewport, full_page)` so unrelated views don't
    /// clobber each other's before/after pair.
    fn baseline_stem(url: &str, viewport_label: &str, full_page: bool) -> String {
        let key = format!("{url}|{viewport_label}|full_page={full_page}");
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    /// `<project>/.copperclaw/baselines/` — sibling of the screenshots dir,
    /// resolved through the same `verify_gate` project-root logic so an
    /// as-yet-uncreated project still resolves (and tests can redirect the
    /// data root).
    fn resolve_baseline_dir(project: Option<&str>) -> PathBuf {
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
        root.join(".copperclaw").join("baselines")
    }

    /// The public hook, mirroring `diagnostics::append_post_edit_digest`:
    /// after a successful PNG capture, diff against this view's stored baseline
    /// (if any) and return the digest note to append; then replace the baseline
    /// with the current capture. Best-effort throughout — any decode/IO failure
    /// yields an empty note and never propagates, so a good screenshot is never
    /// downgraded to an error.
    pub(super) async fn regression_note(
        project: Option<&str>,
        url: &str,
        viewport_label: &str,
        full_page: bool,
        current_png: &[u8],
    ) -> String {
        let dir = resolve_baseline_dir(project);
        let path = dir.join(format!(
            "{}.png",
            baseline_stem(url, viewport_label, full_page)
        ));

        let note = match tokio::fs::read(&path).await {
            Ok(prior) => match (decode_png(&prior), decode_png(current_png)) {
                (Ok(base), Ok(cur)) => {
                    let diff = compute_diff(&base, &cur);
                    // M22 C4 metric: count a flagged visual regression, labelled
                    // by viewport + whether the viewport dimensions themselves
                    // changed (a strong layout-regression signal).
                    if diff.flagged {
                        copperclaw_metrics::inc_visual_regression_flag(
                            viewport_label,
                            diff.dimensions_changed,
                        );
                    }
                    diff_note(&diff)
                }
                // Undecodable baseline or capture (e.g. a jpeg-downgraded prior
                // shot): skip silently and just re-baseline below.
                _ => String::new(),
            },
            Err(_) => String::new(), // no baseline yet — this capture establishes it
        };

        // Re-baseline (create dir best-effort). A write failure must not affect
        // the returned note or the screenshot itself.
        if tokio::fs::create_dir_all(&dir).await.is_ok()
            && tokio::fs::write(&path, current_png).await.is_ok()
        {
            // M22 C4 metric: a view baseline was (re)written for the next diff.
            copperclaw_metrics::inc_visual_regression_baseline();
        }
        note
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn fixture(name: &str) -> Vec<u8> {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/visual_regression/"
            );
            std::fs::read(format!("{path}{name}")).expect("fixture present")
        }

        /// Local data-root override guard so the on-disk baseline tests never
        /// touch the real `/data` root — mirrors the guard the outer
        /// `ui_screenshot` test module keeps for the same reason, held here so
        /// this module stays self-contained.
        struct DataRootGuard {
            _dir: tempfile::TempDir,
            _lock: std::sync::MutexGuard<'static, ()>,
        }

        impl DataRootGuard {
            fn new() -> Self {
                let lock = crate::tools::verify_gate::data_root_test_lock()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let dir = tempfile::tempdir().expect("tempdir");
                crate::tools::verify_gate::data_root_test_override_set(dir.path().to_path_buf());
                Self {
                    _dir: dir,
                    _lock: lock,
                }
            }
        }

        impl Drop for DataRootGuard {
            fn drop(&mut self) {
                crate::tools::verify_gate::data_root_test_override_clear();
            }
        }

        #[test]
        fn decodes_known_rgba_pixels() {
            // decoder_probe.png is a 4x4 RGBA fixture with a hand-chosen
            // pattern — the oracle for the inflate + unfilter path.
            #[rustfmt::skip]
            let expected: [u8; 64] = [
                255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,0,255,
                255,0,255,255, 0,255,255,255, 255,255,255,255, 0,0,0,255,
                10,20,30,255, 40,50,60,128, 70,80,90,255, 100,110,120,64,
                128,128,128,255, 1,2,3,255, 200,201,202,255, 9,8,7,6,
            ];
            let img = decode_png(&fixture("decoder_probe.png")).expect("decode probe");
            assert_eq!((img.width, img.height), (4, 4));
            assert_eq!(img.rgba, expected);
        }

        #[test]
        fn decodes_rgb_color_type_2() {
            // baseline_rgb.png is color type 2 (no alpha) — exercises the
            // 3-channel expansion path; alpha must come back fully opaque.
            let img = decode_png(&fixture("baseline_rgb.png")).expect("decode rgb");
            assert_eq!((img.width, img.height), (160, 120));
            assert!(img.rgba.chunks_exact(4).all(|p| p[3] == 255));
        }

        #[test]
        fn identical_pixels_across_encodings_flag_nothing() {
            // Same pixels, different PNG encoding (PIL optimize=True) — a
            // perceptual diff must see zero change (guards the "identical
            // images → no flag" acceptance).
            let a = decode_png(&fixture("baseline.png")).unwrap();
            let b = decode_png(&fixture("baseline_reencoded.png")).unwrap();
            let d = compute_diff(&a, &b);
            assert!(!d.flagged, "re-encoded identical pixels must not flag");
            assert_eq!(d.changed_regions, 0);
            assert!(diff_note(&d).is_empty());
        }

        #[test]
        fn same_image_against_itself_is_clean() {
            let a = decode_png(&fixture("baseline.png")).unwrap();
            let b = decode_png(&fixture("baseline.png")).unwrap();
            assert!(!compute_diff(&a, &b).flagged);
        }

        #[test]
        fn detects_changed_region_and_localises_it() {
            // regressed.png moves the card down — a localized change the diff
            // must flag, with a bounding box in the lower half of the view.
            let base = decode_png(&fixture("baseline.png")).unwrap();
            let cur = decode_png(&fixture("regressed.png")).unwrap();
            let d = compute_diff(&base, &cur);
            assert!(d.flagged, "a moved card must be flagged");
            assert!(d.changed_regions > 0);
            assert!(!d.dimensions_changed);
            let (_, y0, _, y1) = d.bbox.expect("a bounding box");
            assert!(y1 > y0);
            // The change is confined — not the whole frame.
            assert!(
                d.changed_regions < d.total_regions,
                "a local move must not paint every region as changed"
            );
            let note = diff_note(&d);
            assert!(note.contains("Visual regression"), "{note}");
            assert!(note.contains("region(s) changed"), "{note}");
        }

        #[test]
        fn dimension_change_flags_unconditionally() {
            let a = DecodedImage {
                width: 4,
                height: 4,
                rgba: vec![0u8; 4 * 4 * 4],
            };
            let b = DecodedImage {
                width: 4,
                height: 6,
                rgba: vec![0u8; 4 * 6 * 4],
            };
            let d = compute_diff(&a, &b);
            assert!(d.flagged);
            assert!(d.dimensions_changed);
            assert!(diff_note(&d).contains("dimensions changed"));
        }

        #[test]
        fn decode_rejects_non_png() {
            assert!(decode_png(b"not a png at all").is_err());
        }

        #[tokio::test]
        async fn regression_note_baselines_then_flags_a_regression() {
            // End-to-end through the on-disk baseline path (no chromium):
            // first capture establishes a silent baseline; a follow-up capture
            // of the SAME view with a regressed image is flagged in-turn.
            let g = DataRootGuard::new();
            let url = "http://127.0.0.1:5173/";
            let first =
                regression_note(Some("app"), url, "desktop", false, &fixture("baseline.png")).await;
            assert!(
                first.is_empty(),
                "first capture just sets the baseline: {first}"
            );

            let second = regression_note(
                Some("app"),
                url,
                "desktop",
                false,
                &fixture("regressed.png"),
            )
            .await;
            assert!(
                second.contains("Visual regression"),
                "a regressing follow-up capture must be flagged: {second}"
            );
            drop(g);
        }

        #[tokio::test]
        async fn regression_note_stays_silent_on_an_unchanged_recapture() {
            let g = DataRootGuard::new();
            let url = "http://127.0.0.1:5173/";
            let _ = regression_note(
                Some("app2"),
                url,
                "desktop",
                false,
                &fixture("baseline.png"),
            )
            .await;
            let again = regression_note(
                Some("app2"),
                url,
                "desktop",
                false,
                &fixture("baseline_reencoded.png"),
            )
            .await;
            assert!(
                again.is_empty(),
                "an unchanged re-capture must stay silent: {again}"
            );
            drop(g);
        }
    }
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let prepared = match prepare(&input) {
        Ok(p) => p,
        Err(e) => {
            // M20 M1: `prepare` only fails over URL validation (parse error,
            // bad scheme, or non-loopback host) — all three collapse to the
            // same refused-URL outcome/counter.
            copperclaw_metrics::inc_ui_screenshot("blocked_non_loopback", "desktop");
            copperclaw_metrics::inc_ui_screenshot_refused_url();
            return Err(e);
        }
    };
    let viewport_label = match prepared.capture.viewport {
        Some(ViewportPreset::Mobile) => "mobile",
        _ => "desktop",
    };

    // Probe for chromium AT CALL TIME (not registration time — the tool is
    // always registered under Coding/Full; the minimal profile degrades
    // here, cleanly).
    let Some(binary) = copperclaw_browser::find_chromium_binary() else {
        copperclaw_metrics::inc_ui_screenshot("chromium_missing", viewport_label);
        return Err(chromium_missing_error());
    };

    let transport = match copperclaw_browser::chromium_singleton()
        .get_transport(&binary, NAV_TIMEOUT)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            copperclaw_metrics::inc_ui_screenshot("driver_error", viewport_label);
            return Err(ToolError::Internal(format!(
                "ui_screenshot: could not start the local chromium: {e}"
            )));
        }
    };

    let req = copperclaw_browser::ScreenshotRequest {
        url: prepared.url.to_string(),
        capture: prepared.capture.clone(),
        wait_ms: prepared.wait_ms,
        wait_for_selector: prepared.wait_for_selector.clone(),
        nav_timeout: NAV_TIMEOUT,
    };
    // M20 D2 size safety: an oversize capture auto-retries once as a
    // lower-fidelity jpeg rather than erroring outright.
    let started = std::time::Instant::now();
    let result = match copperclaw_browser::capture_with_size_safety(
        transport.as_ref(),
        &req,
        MAX_SCREENSHOT_BYTES,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            copperclaw_metrics::inc_ui_screenshot("driver_error", viewport_label);
            return Err(ToolError::Internal(format!("ui_screenshot: {e}")));
        }
    };
    copperclaw_metrics::observe_ui_screenshot_capture_seconds(started.elapsed().as_secs_f64());

    if result.bytes.len() as u64 > MAX_SCREENSHOT_BYTES {
        copperclaw_metrics::inc_ui_screenshot("oversize", viewport_label);
        return Err(ToolError::Internal(format!(
            "ui_screenshot: the capture of `{}` was {} bytes, over the {MAX_SCREENSHOT_BYTES}-byte \
             cap even after an automatic retry at a lower-fidelity jpeg — unusual; try again after \
             the page settles (e.g. via `wait_for_selector`), or pass `full_page: false` / a \
             smaller viewport.",
            prepared.url,
            result.bytes.len()
        )));
    }
    copperclaw_metrics::inc_ui_screenshot(
        if result.downgraded {
            "downgraded"
        } else {
            "ok"
        },
        viewport_label,
    );
    copperclaw_metrics::inc_browser_output_format(
        "ui_screenshot",
        result.capture.format.as_cdp_str(),
    );

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

    // M20 D5: fold a console-error count + first error into the response so
    // the common case needs no second `ui_inspect` call. Runtime/Log
    // observation was enabled by `capture()` itself; this just reads back
    // what it buffered on the same tab this call navigated.
    let console_summary = copperclaw_browser::summarize_console(&transport.console_entries());
    if console_summary.error_count > 0 {
        // The folded-in text is page-originated (the page's own console
        // could echo fetched/attacker-influenced content) — taint the turn
        // exactly when such text is actually included, per this tool's
        // module docs.
        ctx.mark_untrusted_context(&format!("ui_screenshot:console:{}", prepared.url));
    }
    let console_note = console_fold_in_note(&console_summary);

    // C4 — screenshot-diff visual regression: diff this capture against the
    // per-view PNG baseline and, if the layout shifted, append a digest note
    // (mirroring C1's post-edit diagnostics feedback). PNG only — the diff's
    // self-contained decoder handles the lossless format the model image
    // captures by default; a jpeg-downgraded shot re-baselines but is not
    // diffed. Best-effort: never turns a good screenshot into an error.
    let visual_note = if ui_diff_enabled() && result.capture.format == ImageFormat::Png {
        visual::regression_note(
            input.project.as_deref(),
            prepared.url.as_str(),
            viewport_label,
            prepared.capture.full_page,
            &result.bytes,
        )
        .await
    } else {
        String::new()
    };

    let b64 = bytes_b64::encode(&result.bytes);
    Ok(CallToolResult::success(vec![
        Content::text(format!(
            "Captured a {width}x{height} {} screenshot of {} ({} bytes){downgrade_note}; saved to \
             {} (use `send_file` to share it) — it is attached below for you to \
             see.{console_note}{visual_note}",
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
        assert!(desc.contains("ui_inspect"), "{desc}");
    }

    // ── M20 D5: console fold-in note ─────────────────────────────────────

    #[test]
    fn console_fold_in_note_is_empty_with_no_errors() {
        let summary = copperclaw_browser::ConsoleSummary::default();
        assert_eq!(console_fold_in_note(&summary), "");
    }

    #[test]
    fn console_fold_in_note_is_empty_with_only_warnings() {
        let summary = copperclaw_browser::ConsoleSummary {
            error_count: 0,
            warning_count: 3,
            first_error: None,
        };
        assert_eq!(
            console_fold_in_note(&summary),
            "",
            "warnings alone must not trigger the fold-in (or the taint)"
        );
    }

    #[test]
    fn console_fold_in_note_reports_count_and_first_error() {
        let summary = copperclaw_browser::ConsoleSummary {
            error_count: 2,
            warning_count: 0,
            first_error: Some("TypeError: x is not a function".to_string()),
        };
        let note = console_fold_in_note(&summary);
        assert!(note.contains("2 error(s)"), "{note}");
        assert!(note.contains("TypeError: x is not a function"), "{note}");
        assert!(note.contains("ui_inspect"), "{note}");
    }

    #[test]
    fn console_fold_in_note_includes_warning_count_alongside_errors() {
        let summary = copperclaw_browser::ConsoleSummary {
            error_count: 1,
            warning_count: 2,
            first_error: Some("boom".to_string()),
        };
        let note = console_fold_in_note(&summary);
        assert!(note.contains("1 error(s)"), "{note}");
        assert!(note.contains("2 warning(s)"), "{note}");
    }

    // ── C4: visual-regression opt-out flag ───────────────────────────────

    #[test]
    fn ui_diff_flag_defaults_on_and_honours_opt_out() {
        assert!(parse_ui_diff_flag(None));
        assert!(parse_ui_diff_flag(Some("1")));
        assert!(parse_ui_diff_flag(Some("yes")));
        for off in ["0", "false", "off", "no", " OFF ", "False"] {
            assert!(
                !parse_ui_diff_flag(Some(off)),
                "`{off}` should disable the diff"
            );
        }
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
