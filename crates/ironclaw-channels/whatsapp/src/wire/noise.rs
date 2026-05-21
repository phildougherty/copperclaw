//! Noise XX handshake state machine for the WhatsApp WebSocket.
//!
//! The WhatsApp Web client opens a TCP/TLS WebSocket and then runs a
//! Noise XX handshake over the framing in [`crate::wire::frame`]. The
//! standard XX pattern is:
//!
//! ```text
//! -> e
//! <- e, ee, s, es
//! -> s, se
//! (transport mode)
//! ```
//!
//! From the initiator's point of view the state machine therefore has
//! three forward transitions:
//!
//! 1. [`NoiseState::Initial`] -> generate ephemeral `e`, send it,
//!    advance to [`NoiseState::SentE`].
//! 2. [`NoiseState::SentE`] -> receive `e, ee, s, es`, decrypt the
//!    server's static key, advance to [`NoiseState::ReceivedSE`].
//! 3. [`NoiseState::ReceivedSE`] -> send the client static key + `se`,
//!    advance to [`NoiseState::Done`].
//!
//! This module owns the state and the message ordering; the actual
//! Curve25519 / HKDF / AEAD calls are delegated to a [`CryptoBackend`]
//! the caller supplies. A future contributor wiring real crypto only
//! needs to provide a backend that implements the [`CryptoBackend`]
//! primitives.
//!
//! ### What is and isn't here
//!
//! - Tested: state-machine transitions, error paths (out-of-order
//!   messages, double-finish, attempts to use the machine after it
//!   completes), the trait surface for [`CryptoBackend`] driven
//!   primitives, and the formatting of every variant.
//! - Not implemented: any cryptographic operation. The stub backend in
//!   `crate::crypto::stub` returns
//!   [`crate::crypto::CryptoError::NotImplemented`] for every primitive;
//!   `HandshakeMachine` propagates those errors verbatim.

use crate::crypto::{CryptoBackend, CryptoError};

/// State of the XX handshake from the initiator's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseState {
    /// Nothing has happened yet. Next step: [`HandshakeMachine::start`].
    Initial,
    /// The client has sent its ephemeral `e`. Next step:
    /// [`HandshakeMachine::receive_server_hello`].
    SentE,
    /// The client has processed the server's `e, ee, s, es` payload.
    /// Next step: [`HandshakeMachine::finish`].
    ReceivedSE,
    /// The handshake is complete. The machine is no longer usable.
    Done,
    /// The handshake produced an unrecoverable error. Carries the
    /// stringified description.
    Failed,
}

impl NoiseState {
    /// `true` when the machine is in a state that can still progress.
    pub fn is_pending(self) -> bool {
        !matches!(self, Self::Done | Self::Failed)
    }

    /// `true` when the handshake completed successfully.
    pub fn is_done(self) -> bool {
        matches!(self, Self::Done)
    }
}

/// Errors emitted by [`HandshakeMachine`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NoiseError {
    /// The caller invoked a method that does not apply to the current
    /// state. Carries `(observed_state, attempted_step)`.
    #[error("invalid noise step `{step}` for state {state:?}")]
    InvalidStep {
        state: NoiseState,
        step: &'static str,
    },
    /// The underlying crypto backend rejected an operation.
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    /// The server's message did not match the expected XX layout.
    #[error("malformed server message: {0}")]
    MalformedMessage(String),
}

/// Bytes the server sent during the XX handshake.
///
/// `payload` is the full ciphertext blob the server published; the state
/// machine hands it to the crypto backend for decryption and parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub payload: Vec<u8>,
}

/// The output of a successful handshake.
///
/// `send_key` and `recv_key` are the two AEAD session keys. `handshake_hash`
/// is the symmetric state's final hash, used to bind the channel to the
/// next layer's identity proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeOutput {
    pub send_key: Vec<u8>,
    pub recv_key: Vec<u8>,
    pub handshake_hash: Vec<u8>,
}

/// XX handshake driver from the initiator's point of view.
///
/// Construct with [`HandshakeMachine::new`], then drive it through
/// `start` -> `receive_server_hello` -> `finish`. The machine is single-
/// use; once it reaches [`NoiseState::Done`] or [`NoiseState::Failed`] it
/// rejects further calls.
pub struct HandshakeMachine<B: CryptoBackend> {
    backend: B,
    state: NoiseState,
    /// Bytes the client most recently sent. Stored so the test surface
    /// can introspect it; not used for any state transition.
    last_sent: Vec<u8>,
}

impl<B: CryptoBackend> std::fmt::Debug for HandshakeMachine<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandshakeMachine")
            .field("state", &self.state)
            .field("last_sent_bytes", &self.last_sent.len())
            .finish_non_exhaustive()
    }
}

impl<B: CryptoBackend> HandshakeMachine<B> {
    /// Build a fresh handshake in the [`NoiseState::Initial`] state.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: NoiseState::Initial,
            last_sent: Vec::new(),
        }
    }

    /// Current state.
    pub fn state(&self) -> NoiseState {
        self.state
    }

    /// Latest bytes the client sent.
    pub fn last_sent(&self) -> &[u8] {
        &self.last_sent
    }

    /// Owned access to the underlying backend (mostly useful for tests).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Step 1: generate the client ephemeral `e` and produce the first
    /// handshake frame to send to the server.
    ///
    /// On success, advances to [`NoiseState::SentE`] and returns the
    /// bytes the caller should put on the wire.
    pub fn start(&mut self) -> Result<Vec<u8>, NoiseError> {
        if self.state != NoiseState::Initial {
            return Err(NoiseError::InvalidStep {
                state: self.state,
                step: "start",
            });
        }
        let kp = self.backend.generate_keypair().inspect_err(|_| {
            self.state = NoiseState::Failed;
        })?;
        let mut bytes = Vec::with_capacity(kp.public.len() + 1);
        bytes.push(MSG_KIND_E);
        bytes.extend_from_slice(&kp.public);
        self.last_sent.clone_from(&bytes);
        self.state = NoiseState::SentE;
        Ok(bytes)
    }

    /// Step 2: feed the server's `e, ee, s, es` payload.
    ///
    /// The caller is expected to have stripped the WebSocket framing
    /// already. The machine asks the backend to perform the DH and
    /// HKDF operations the XX pattern prescribes; it does not interpret
    /// the bytes itself beyond a basic header check.
    pub fn receive_server_hello(&mut self, hello: &ServerHello) -> Result<(), NoiseError> {
        if self.state != NoiseState::SentE {
            return Err(NoiseError::InvalidStep {
                state: self.state,
                step: "receive_server_hello",
            });
        }
        if hello.payload.is_empty() {
            self.state = NoiseState::Failed;
            return Err(NoiseError::MalformedMessage(
                "server hello is empty".into(),
            ));
        }
        if hello.payload[0] != MSG_KIND_E_EE_S_ES {
            self.state = NoiseState::Failed;
            return Err(NoiseError::MalformedMessage(format!(
                "expected message kind 0x{MSG_KIND_E_EE_S_ES:02x}, got 0x{:02x}",
                hello.payload[0]
            )));
        }
        // Real implementation: split payload into `e || enc(s) || tag` and
        // run XX's `ee, es` mixes through the backend's `dh` + `hkdf_*`
        // + `aead_open` primitives. The stub backend returns errors here.
        let _ = self
            .backend
            .dh(&[], &hello.payload)
            .inspect_err(|_| {
                self.state = NoiseState::Failed;
            })?;
        self.state = NoiseState::ReceivedSE;
        Ok(())
    }

    /// Step 3: emit the final handshake message and derive session keys.
    pub fn finish(&mut self) -> Result<HandshakeOutput, NoiseError> {
        if self.state != NoiseState::ReceivedSE {
            return Err(NoiseError::InvalidStep {
                state: self.state,
                step: "finish",
            });
        }
        // Real implementation: emit `s, se`, mix `se`, derive session keys
        // via `hkdf_expand`. The stub backend errors at the first DH.
        let _ = self
            .backend
            .dh(&[], &[])
            .inspect_err(|_| {
                self.state = NoiseState::Failed;
            })?;
        let send_key = self
            .backend
            .hkdf_expand(&[], &[], 32)
            .inspect_err(|_| {
                self.state = NoiseState::Failed;
            })?;
        let recv_key = self
            .backend
            .hkdf_expand(&[], &[], 32)
            .inspect_err(|_| {
                self.state = NoiseState::Failed;
            })?;
        self.state = NoiseState::Done;
        Ok(HandshakeOutput {
            send_key,
            recv_key,
            handshake_hash: vec![],
        })
    }
}

/// First message kind: client sends its ephemeral.
pub const MSG_KIND_E: u8 = 0x01;
/// Second message kind: server sends `e, ee, s, es`.
pub const MSG_KIND_E_EE_S_ES: u8 = 0x02;
/// Third message kind: client sends `s, se`.
pub const MSG_KIND_S_SE: u8 = 0x03;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::stub::StubBackend;
    use crate::crypto::{CryptoBackend, KeyPair};

    /// A scriptable backend for the noise tests: each primitive returns
    /// either a queued success or a queued error in FIFO order.
    #[derive(Default)]
    #[allow(clippy::struct_field_names)]
    struct ScriptedBackend {
        keypair_queue: std::sync::Mutex<Vec<Result<KeyPair, CryptoError>>>,
        dh_queue: std::sync::Mutex<Vec<Result<Vec<u8>, CryptoError>>>,
        hkdf_queue: std::sync::Mutex<Vec<Result<Vec<u8>, CryptoError>>>,
    }

    impl ScriptedBackend {
        fn queue_keypair(&self, r: Result<KeyPair, CryptoError>) {
            self.keypair_queue.lock().unwrap().push(r);
        }
        fn queue_dh(&self, r: Result<Vec<u8>, CryptoError>) {
            self.dh_queue.lock().unwrap().push(r);
        }
        fn queue_hkdf(&self, r: Result<Vec<u8>, CryptoError>) {
            self.hkdf_queue.lock().unwrap().push(r);
        }
    }

    impl CryptoBackend for ScriptedBackend {
        fn generate_keypair(&self) -> Result<KeyPair, CryptoError> {
            self.keypair_queue
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(CryptoError::NotImplemented("generate_keypair")))
        }
        fn dh(&self, _priv_key: &[u8], _pub_key: &[u8]) -> Result<Vec<u8>, CryptoError> {
            self.dh_queue
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(CryptoError::NotImplemented("dh")))
        }
        fn hkdf_extract(&self, _salt: &[u8], _ikm: &[u8]) -> Result<Vec<u8>, CryptoError> {
            Err(CryptoError::NotImplemented("hkdf_extract"))
        }
        fn hkdf_expand(
            &self,
            _prk: &[u8],
            _info: &[u8],
            _length: usize,
        ) -> Result<Vec<u8>, CryptoError> {
            self.hkdf_queue
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(CryptoError::NotImplemented("hkdf_expand")))
        }
        fn aead_seal(
            &self,
            _key: &[u8],
            _nonce: &[u8],
            _ad: &[u8],
            _plaintext: &[u8],
        ) -> Result<Vec<u8>, CryptoError> {
            Err(CryptoError::NotImplemented("aead_seal"))
        }
        fn aead_open(
            &self,
            _key: &[u8],
            _nonce: &[u8],
            _ad: &[u8],
            _ciphertext: &[u8],
        ) -> Result<Vec<u8>, CryptoError> {
            Err(CryptoError::NotImplemented("aead_open"))
        }
        fn sign(&self, _priv_key: &[u8], _data: &[u8]) -> Result<Vec<u8>, CryptoError> {
            Err(CryptoError::NotImplemented("sign"))
        }
        fn verify(&self, _pub_key: &[u8], _data: &[u8], _sig: &[u8]) -> Result<bool, CryptoError> {
            Err(CryptoError::NotImplemented("verify"))
        }
    }

    // ---- state predicates ----

    #[test]
    fn pending_predicate() {
        for s in [NoiseState::Initial, NoiseState::SentE, NoiseState::ReceivedSE] {
            assert!(s.is_pending(), "{s:?} should be pending");
            assert!(!s.is_done(), "{s:?} should not be done");
        }
        assert!(!NoiseState::Done.is_pending());
        assert!(NoiseState::Done.is_done());
        assert!(!NoiseState::Failed.is_pending());
        assert!(!NoiseState::Failed.is_done());
    }

    #[test]
    fn state_clone_eq_debug() {
        let s = NoiseState::SentE;
        let copy = s;
        assert_eq!(s, copy);
        assert!(format!("{s:?}").contains("SentE"));
    }

    // ---- Initial transitions ----

    #[test]
    fn new_starts_in_initial_state() {
        let m = HandshakeMachine::new(StubBackend);
        assert_eq!(m.state(), NoiseState::Initial);
        assert_eq!(m.last_sent(), b"");
    }

    #[test]
    fn start_with_stub_backend_errors_and_marks_failed() {
        let mut m = HandshakeMachine::new(StubBackend);
        let err = m.start().unwrap_err();
        assert!(matches!(err, NoiseError::Crypto(_)));
        assert_eq!(m.state(), NoiseState::Failed);
    }

    #[test]
    fn start_with_scripted_keypair_advances_to_sent_e() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![0x11, 0x22, 0x33],
            private: vec![0x44, 0x55, 0x66],
        }));
        let mut m = HandshakeMachine::new(backend);
        let bytes = m.start().unwrap();
        assert_eq!(m.state(), NoiseState::SentE);
        assert_eq!(bytes[0], MSG_KIND_E);
        assert_eq!(&bytes[1..], &[0x11, 0x22, 0x33]);
        // last_sent mirrors the wire bytes.
        assert_eq!(m.last_sent(), bytes.as_slice());
    }

    #[test]
    fn start_twice_errors_with_invalid_step() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        let mut m = HandshakeMachine::new(backend);
        let _ = m.start().unwrap();
        let err = m.start().unwrap_err();
        match err {
            NoiseError::InvalidStep { state, step } => {
                assert_eq!(state, NoiseState::SentE);
                assert_eq!(step, "start");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- SentE transitions ----

    #[test]
    fn receive_server_hello_advances_to_received_se() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        backend.queue_dh(Ok(vec![0xAB; 32]));
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        let hello = ServerHello {
            payload: vec![MSG_KIND_E_EE_S_ES, 0x01, 0x02, 0x03],
        };
        m.receive_server_hello(&hello).unwrap();
        assert_eq!(m.state(), NoiseState::ReceivedSE);
    }

    #[test]
    fn receive_server_hello_before_start_is_invalid_step() {
        let mut m = HandshakeMachine::new(StubBackend);
        let hello = ServerHello {
            payload: vec![MSG_KIND_E_EE_S_ES, 0x01],
        };
        let err = m.receive_server_hello(&hello).unwrap_err();
        match err {
            NoiseError::InvalidStep { state, step } => {
                assert_eq!(state, NoiseState::Initial);
                assert_eq!(step, "receive_server_hello");
            }
            other => panic!("unexpected: {other:?}"),
        }
        // State is unchanged.
        assert_eq!(m.state(), NoiseState::Initial);
    }

    #[test]
    fn receive_empty_server_hello_marks_failed() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        let err = m
            .receive_server_hello(&ServerHello { payload: vec![] })
            .unwrap_err();
        assert!(matches!(err, NoiseError::MalformedMessage(_)));
        assert_eq!(m.state(), NoiseState::Failed);
    }

    #[test]
    fn receive_server_hello_with_wrong_kind_byte_marks_failed() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        let err = m
            .receive_server_hello(&ServerHello {
                payload: vec![0x99],
            })
            .unwrap_err();
        match err {
            NoiseError::MalformedMessage(s) => assert!(s.contains("0x99")),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(m.state(), NoiseState::Failed);
    }

    #[test]
    fn receive_server_hello_propagates_dh_error() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        // dh result not queued -> ScriptedBackend.dh defaults to NotImplemented.
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        let err = m
            .receive_server_hello(&ServerHello {
                payload: vec![MSG_KIND_E_EE_S_ES, 0x10],
            })
            .unwrap_err();
        assert!(matches!(err, NoiseError::Crypto(_)));
        assert_eq!(m.state(), NoiseState::Failed);
    }

    // ---- ReceivedSE transitions ----

    fn fully_advanced_machine() -> HandshakeMachine<ScriptedBackend> {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        backend.queue_dh(Ok(vec![0xAB; 32])); // for hello
        backend.queue_dh(Ok(vec![0xCD; 32])); // for finish
        backend.queue_hkdf(Ok(vec![0xEF; 32]));
        backend.queue_hkdf(Ok(vec![0x10; 32]));
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        m.receive_server_hello(&ServerHello {
            payload: vec![MSG_KIND_E_EE_S_ES, 0xAA],
        })
        .unwrap();
        m
    }

    #[test]
    fn finish_after_receive_se_yields_keys_and_done_state() {
        let mut m = fully_advanced_machine();
        let out = m.finish().unwrap();
        assert_eq!(m.state(), NoiseState::Done);
        assert_eq!(out.send_key.len(), 32);
        assert_eq!(out.recv_key.len(), 32);
        // Default ScriptedBackend.queue_hkdf pops in LIFO order; the order
        // here isn't load-bearing, but we know each call returned 32 bytes.
    }

    #[test]
    fn finish_before_receive_se_is_invalid_step() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        let err = m.finish().unwrap_err();
        match err {
            NoiseError::InvalidStep { state, step } => {
                assert_eq!(state, NoiseState::SentE);
                assert_eq!(step, "finish");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn finish_from_initial_state_is_invalid_step() {
        let mut m = HandshakeMachine::new(StubBackend);
        let err = m.finish().unwrap_err();
        assert!(matches!(err, NoiseError::InvalidStep { .. }));
    }

    #[test]
    fn finish_twice_errors_with_invalid_step() {
        let mut m = fully_advanced_machine();
        m.finish().unwrap();
        let err = m.finish().unwrap_err();
        match err {
            NoiseError::InvalidStep { state, step } => {
                assert_eq!(state, NoiseState::Done);
                assert_eq!(step, "finish");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn finish_propagates_dh_error() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        backend.queue_dh(Ok(vec![0xAB; 32]));
        // no dh queued for finish -> NotImplemented.
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        m.receive_server_hello(&ServerHello {
            payload: vec![MSG_KIND_E_EE_S_ES, 0x10],
        })
        .unwrap();
        let err = m.finish().unwrap_err();
        assert!(matches!(err, NoiseError::Crypto(_)));
        assert_eq!(m.state(), NoiseState::Failed);
    }

    #[test]
    fn finish_propagates_hkdf_error_after_dh_ok() {
        let backend = ScriptedBackend::default();
        backend.queue_keypair(Ok(KeyPair {
            public: vec![1],
            private: vec![2],
        }));
        backend.queue_dh(Ok(vec![0xAB; 32]));
        backend.queue_dh(Ok(vec![0xCD; 32]));
        // no hkdf queued -> NotImplemented.
        let mut m = HandshakeMachine::new(backend);
        m.start().unwrap();
        m.receive_server_hello(&ServerHello {
            payload: vec![MSG_KIND_E_EE_S_ES, 0xFF],
        })
        .unwrap();
        let err = m.finish().unwrap_err();
        assert!(matches!(err, NoiseError::Crypto(_)));
        assert_eq!(m.state(), NoiseState::Failed);
    }

    // ---- Failed-state guards ----

    #[test]
    fn after_failure_start_still_returns_invalid_step() {
        let mut m = HandshakeMachine::new(StubBackend);
        let _ = m.start(); // fails because stub keypair errors
        let err = m.start().unwrap_err();
        assert!(matches!(err, NoiseError::InvalidStep { state: NoiseState::Failed, .. }));
    }

    #[test]
    fn after_failure_receive_returns_invalid_step() {
        let mut m = HandshakeMachine::new(StubBackend);
        let _ = m.start();
        let err = m
            .receive_server_hello(&ServerHello { payload: vec![1] })
            .unwrap_err();
        assert!(matches!(err, NoiseError::InvalidStep { state: NoiseState::Failed, .. }));
    }

    #[test]
    fn after_failure_finish_returns_invalid_step() {
        let mut m = HandshakeMachine::new(StubBackend);
        let _ = m.start();
        let err = m.finish().unwrap_err();
        assert!(matches!(err, NoiseError::InvalidStep { state: NoiseState::Failed, .. }));
    }

    // ---- accessors / debug ----

    #[test]
    fn backend_accessor_returns_reference() {
        let m = HandshakeMachine::new(StubBackend);
        let _: &StubBackend = m.backend();
    }

    #[test]
    fn debug_format_renders_state_and_sent_len() {
        let m = HandshakeMachine::new(StubBackend);
        let s = format!("{m:?}");
        assert!(s.contains("HandshakeMachine"));
        assert!(s.contains("Initial"));
        assert!(s.contains("last_sent_bytes"));
    }

    // ---- ServerHello / HandshakeOutput ----

    #[test]
    fn server_hello_clone_eq_debug() {
        let a = ServerHello {
            payload: vec![1, 2],
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("ServerHello"));
    }

    #[test]
    fn handshake_output_clone_eq_debug() {
        let a = HandshakeOutput {
            send_key: vec![1],
            recv_key: vec![2],
            handshake_hash: vec![3],
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("HandshakeOutput"));
    }

    // ---- NoiseError ----

    #[test]
    fn noise_error_display_invalid_step() {
        let e = NoiseError::InvalidStep {
            state: NoiseState::Initial,
            step: "start",
        };
        let s = format!("{e}");
        assert!(s.contains("start"));
        assert!(s.contains("Initial"));
    }

    #[test]
    fn noise_error_display_malformed() {
        let e = NoiseError::MalformedMessage("bad".into());
        assert!(format!("{e}").contains("bad"));
    }

    #[test]
    fn noise_error_from_crypto_error() {
        let c = CryptoError::NotImplemented("x");
        let e: NoiseError = c.into();
        assert!(matches!(e, NoiseError::Crypto(_)));
    }

    #[test]
    fn noise_error_eq_and_debug() {
        let a = NoiseError::MalformedMessage("a".into());
        let b = NoiseError::MalformedMessage("a".into());
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("MalformedMessage"));
    }

    // ---- constants ----

    #[test]
    fn message_kind_constants_are_distinct() {
        assert_ne!(MSG_KIND_E, MSG_KIND_E_EE_S_ES);
        assert_ne!(MSG_KIND_E_EE_S_ES, MSG_KIND_S_SE);
        assert_eq!(MSG_KIND_E, 0x01);
        assert_eq!(MSG_KIND_E_EE_S_ES, 0x02);
        assert_eq!(MSG_KIND_S_SE, 0x03);
    }
}
