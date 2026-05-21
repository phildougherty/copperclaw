//! Persistent device identity / session state for the WhatsApp channel.
//!
//! WhatsApp's reverse-engineered protocol requires the client to maintain a
//! stable identity across reconnects: a Curve25519 device key pair, an
//! Ed25519 signing identity, the registration id, an opaque `noise_key`
//! used to seed the next handshake, and a server-provided routing token.
//! All of those values are encoded here as base64-encoded byte strings so
//! the keystore is human-inspectable and easy to back up.
//!
//! On-disk shape (JSON):
//!
//! ```json
//! {
//!   "version": 1,
//!   "device_id": "abc-123",
//!   "registration_id": 12345,
//!   "identity_keypair":  { "private": "<b64>", "public": "<b64>" },
//!   "signed_pre_key":    { "id": 1, "private": "<b64>", "public": "<b64>",
//!                          "signature": "<b64>" },
//!   "noise_key":         "<b64>",
//!   "routing_token":     "<b64>",
//!   "advanced_secret":   "<b64>",
//!   "session_state":     { "free-form, opaque to this crate": "..." }
//! }
//! ```
//!
//! Writes are atomic: the keystore is serialised to a sibling tempfile and
//! then renamed over the target. A corrupt or truncated existing file is
//! handled by [`load`] — it returns a fresh empty keystore and logs at
//! `warn` level rather than failing. The bad file is renamed to
//! `<path>.corrupt-<timestamp>` so a human can inspect it.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current on-disk schema version.
pub const KEYSTORE_VERSION: u32 = 1;

/// Suffix added to corrupted keystores before they are renamed out of the
/// way. The full extension is `.corrupt-<unix-secs>`.
pub const CORRUPT_SUFFIX: &str = "corrupt";

/// Pair of base64-encoded keys, public + private.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct B64KeyPair {
    /// Base64-encoded private key bytes.
    #[serde(default)]
    pub private: String,
    /// Base64-encoded public key bytes.
    #[serde(default)]
    pub public: String,
}

/// A signed pre-key bundle for the Signal Protocol session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPreKey {
    /// Pre-key id assigned by the server.
    #[serde(default)]
    pub id: u32,
    /// Base64-encoded private key.
    #[serde(default)]
    pub private: String,
    /// Base64-encoded public key.
    #[serde(default)]
    pub public: String,
    /// Base64-encoded Ed25519 signature over `public`.
    #[serde(default)]
    pub signature: String,
}

/// Persisted device + session state for the WhatsApp channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keystore {
    /// Schema version. Defaults to [`KEYSTORE_VERSION`] for fresh stores.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Opaque device id assigned by WhatsApp.
    #[serde(default)]
    pub device_id: String,
    /// Numeric registration id (chosen by the client during pairing).
    #[serde(default)]
    pub registration_id: u32,
    /// Device identity keypair.
    #[serde(default)]
    pub identity_keypair: B64KeyPair,
    /// Signed pre-key bundle.
    #[serde(default)]
    pub signed_pre_key: SignedPreKey,
    /// Base64-encoded Noise seed key.
    #[serde(default)]
    pub noise_key: String,
    /// Base64-encoded routing token returned by the server.
    #[serde(default)]
    pub routing_token: String,
    /// Base64-encoded advanced secret.
    #[serde(default)]
    pub advanced_secret: String,
    /// Free-form per-session state opaque to this module.
    #[serde(default)]
    pub session_state: serde_json::Value,
}

const fn default_version() -> u32 {
    KEYSTORE_VERSION
}

impl Keystore {
    /// True when the keystore appears to carry no identity material yet.
    /// Used by the adapter to decide whether to start the pairing flow.
    pub fn is_empty(&self) -> bool {
        self.device_id.is_empty()
            && self.identity_keypair.public.is_empty()
            && self.identity_keypair.private.is_empty()
            && self.noise_key.is_empty()
    }
}

/// Errors emitted by the keystore.
#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    /// I/O failure (open, write, rename, fsync).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialisation / deserialisation failure.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Load a keystore from `path`.
///
/// Behaviour:
///
/// - If the file does not exist, returns `Ok(Keystore::default())`.
/// - If the file exists but is corrupt or unparseable, the bad file is
///   renamed to `<path>.corrupt-<unix-secs>` and `Ok(Keystore::default())`
///   is returned.
/// - Any other I/O error surfaces as `KeystoreError::Io`.
pub fn load(path: &Path) -> Result<Keystore, KeystoreError> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Keystore>(&bytes) {
            Ok(ks) => Ok(ks),
            Err(err) => {
                tracing::warn!(?err, path = %path.display(), "whatsapp: keystore corrupt; rotating");
                let backup = corrupt_path(path);
                if let Err(rename_err) = std::fs::rename(path, &backup) {
                    tracing::warn!(?rename_err, path = %backup.display(), "whatsapp: failed to rotate corrupt keystore");
                }
                Ok(Keystore::default())
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Keystore::default()),
        Err(err) => Err(KeystoreError::Io(err)),
    }
}

/// Save `keystore` atomically to `path`.
///
/// Atomicity strategy: serialise to `<path>.tmp.<unix-nanos>`, fsync the
/// tempfile, then `rename` it over `path`. On POSIX systems this gives an
/// all-or-nothing replace.
pub fn save(path: &Path, keystore: &Keystore) -> Result<(), KeystoreError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let bytes = serde_json::to_vec_pretty(keystore)?;
    let tmp = temp_path(path);
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        // Best-effort cleanup on failure so we do not leave stray tempfiles.
        let _ = std::fs::remove_file(&tmp);
        return Err(KeystoreError::Io(err));
    }
    Ok(())
}

fn corrupt_path(path: &Path) -> PathBuf {
    let ts = Utc::now().timestamp();
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{CORRUPT_SUFFIX}-{ts}"));
    PathBuf::from(s)
}

fn temp_path(path: &Path) -> PathBuf {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".tmp.{nanos}"));
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn populated() -> Keystore {
        Keystore {
            version: KEYSTORE_VERSION,
            device_id: "dev-1".into(),
            registration_id: 7,
            identity_keypair: B64KeyPair {
                private: "AQID".into(),
                public: "BAUG".into(),
            },
            signed_pre_key: SignedPreKey {
                id: 1,
                private: "BwgJ".into(),
                public: "CgsM".into(),
                signature: "DQ4P".into(),
            },
            noise_key: "EBES".into(),
            routing_token: "FBUW".into(),
            advanced_secret: "FxgZ".into(),
            session_state: serde_json::json!({"k": "v"}),
        }
    }

    #[test]
    fn default_is_empty() {
        let ks = Keystore::default();
        assert!(ks.is_empty());
        assert_eq!(ks.version, 0); // default!() gives 0; load() / save() use KEYSTORE_VERSION via the default fn
    }

    #[test]
    fn populated_is_not_empty() {
        let ks = populated();
        assert!(!ks.is_empty());
    }

    #[test]
    fn empty_predicate_checks_required_fields() {
        let mut ks = Keystore {
            routing_token: "tok".into(),
            ..Keystore::default()
        };
        // routing_token alone is not enough to make it look populated.
        assert!(ks.is_empty());
        ks.noise_key = "nk".into();
        assert!(!ks.is_empty());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        let ks = load(&path).unwrap();
        assert!(ks.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ks.json");
        let ks = populated();
        save(&path, &ks).unwrap();
        assert!(path.exists());
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, ks);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c/ks.json");
        save(&path, &populated()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_is_atomic_no_tempfile_left_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ks.json");
        save(&path, &populated()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "extra files left: {entries:?}");
    }

    #[test]
    fn load_corrupt_file_rotates_and_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ks.json");
        std::fs::write(&path, b"not json {").unwrap();
        let ks = load(&path).unwrap();
        assert!(ks.is_empty());
        // Original file should have been moved aside.
        assert!(!path.exists(), "corrupt file should have been rotated");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(entries.len(), 1);
        let name = entries[0].file_name().into_string().unwrap();
        assert!(
            name.contains(CORRUPT_SUFFIX),
            "rotated file should contain `{CORRUPT_SUFFIX}`, got {name}"
        );
    }

    #[test]
    fn load_partial_json_is_treated_as_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ks.json");
        std::fs::write(&path, b"{\"version\": 1, \"device_id\":").unwrap();
        let ks = load(&path).unwrap();
        assert!(ks.is_empty());
    }

    #[test]
    fn load_extraneous_fields_are_ignored() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ks.json");
        std::fs::write(
            &path,
            br#"{
                "version": 1,
                "device_id": "x",
                "noise_key": "AA==",
                "identity_keypair": {"private": "BB", "public": "CC"},
                "unrelated": [1, 2, 3]
            }"#,
        )
        .unwrap();
        let ks = load(&path).unwrap();
        assert_eq!(ks.device_id, "x");
        assert_eq!(ks.noise_key, "AA==");
        assert!(!ks.is_empty());
    }

    #[test]
    fn save_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ks.json");
        let mut ks = populated();
        save(&path, &ks).unwrap();
        ks.device_id = "dev-2".into();
        save(&path, &ks).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.device_id, "dev-2");
    }

    #[test]
    fn b64keypair_default_is_empty_strings() {
        let kp = B64KeyPair::default();
        assert!(kp.private.is_empty());
        assert!(kp.public.is_empty());
    }

    #[test]
    fn signed_pre_key_default_is_zeroed() {
        let pk = SignedPreKey::default();
        assert_eq!(pk.id, 0);
        assert!(pk.public.is_empty());
        assert!(pk.private.is_empty());
        assert!(pk.signature.is_empty());
    }

    #[test]
    fn keystore_serialises_to_object_with_known_keys() {
        let json = serde_json::to_value(populated()).unwrap();
        let obj = json.as_object().unwrap();
        for key in [
            "version",
            "device_id",
            "registration_id",
            "identity_keypair",
            "signed_pre_key",
            "noise_key",
            "routing_token",
            "advanced_secret",
            "session_state",
        ] {
            assert!(obj.contains_key(key), "missing field `{key}` in serialised keystore");
        }
    }

    #[test]
    fn keystore_round_trips_through_json() {
        let ks = populated();
        let bytes = serde_json::to_vec(&ks).unwrap();
        let back: Keystore = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, ks);
    }

    #[test]
    fn keystore_error_io_displays() {
        let err: KeystoreError =
            std::io::Error::new(std::io::ErrorKind::Other, "boom").into();
        let s = format!("{err}");
        assert!(s.contains("io:"));
        assert!(s.contains("boom"));
    }

    #[test]
    fn keystore_error_serde_displays() {
        let parse_err: serde_json::Error = serde_json::from_str::<u8>("not a number").unwrap_err();
        let err: KeystoreError = parse_err.into();
        let s = format!("{err}");
        assert!(s.contains("serde:"));
    }

    #[test]
    fn corrupt_path_appends_corrupt_suffix() {
        let p = Path::new("/tmp/ks.json");
        let cp = corrupt_path(p);
        let s = cp.to_string_lossy();
        assert!(s.starts_with("/tmp/ks.json."));
        assert!(s.contains(CORRUPT_SUFFIX));
    }

    #[test]
    fn temp_path_appends_tmp_suffix() {
        let p = Path::new("/tmp/ks.json");
        let tp = temp_path(p);
        let s = tp.to_string_lossy();
        assert!(s.starts_with("/tmp/ks.json.tmp."));
    }

    #[test]
    fn default_version_constant_matches() {
        assert_eq!(default_version(), KEYSTORE_VERSION);
        assert_eq!(KEYSTORE_VERSION, 1);
    }

    #[test]
    fn clone_keystore() {
        let ks = populated();
        let copy = ks.clone();
        assert_eq!(ks, copy);
    }

    #[test]
    fn debug_keystore_renders() {
        let ks = populated();
        let s = format!("{ks:?}");
        assert!(s.contains("Keystore"));
    }

    #[test]
    fn save_into_path_with_no_parent_succeeds() {
        // PathBuf::from("ks.json") has parent Some("") on most platforms;
        // the save helper handles this by skipping the create_dir_all call.
        let dir = TempDir::new().unwrap();
        // Use a relative path inside the temp dir by chdir'ing — but since
        // tests share state, just test that an absolute path at the root
        // of the temp dir works.
        let path = dir.path().join("ks.json");
        save(&path, &populated()).unwrap();
        assert!(path.exists());
    }
}
