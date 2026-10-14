//! Browser bindings for Nuée.
//!
//! The wasm-bindgen API (module `wasm`, wasm32 only) runs inside one dedicated Web Worker,
//! never on the page's main thread; its JavaScript contract is `web/app/api.d.ts`. This
//! module holds the target-independent part (link summaries, error codes, names shared with
//! JavaScript), so it is also built and linted natively.

use nuee_net::NetError;
use nuee_proto::{
    ByeReason, EndpointId, FailKind, Link, LinkError, ModuleInfo, TaskKind, ValidateError,
};
use nuee_store::StoreError;

#[cfg(target_family = "wasm")]
mod wasm;

/// Version of this package, reported by `version()`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Error codes shared with JavaScript; the UI chooses its message from the code only.
pub mod code {
    /// The relay did not accept us in time.
    pub const RELAY_UNREACHABLE: &str = "relay_unreachable";
    /// The swarm or provider is gone or cannot be reached.
    pub const SWARM_ENDED: &str = "swarm_ended";
    /// The link is incomplete.
    pub const LINK_TRUNCATED: &str = "link_truncated";
    /// The link comes from a newer version.
    pub const LINK_VERSION: &str = "link_version";
    /// The link is damaged.
    pub const LINK_CORRUPT: &str = "link_corrupt";
    /// The link is of another kind than the action needs.
    pub const LINK_KIND: &str = "link_kind";
    /// The coordinator or provider refused us.
    pub const ADMISSION_REFUSED: &str = "admission_refused";
    /// The received bytes do not match the link.
    pub const INTEGRITY: &str = "integrity";
    /// The content cannot be decrypted.
    pub const DECRYPT: &str = "decrypt";
    /// Above a size limit.
    pub const TOO_LARGE: &str = "too_large";
    /// Not available in this build or in a browser.
    pub const UNSUPPORTED: &str = "unsupported";
    /// The task module breaks the task ABI.
    pub const INVALID_MODULE: &str = "invalid_module";
    /// The caller passed a malformed argument.
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    /// Anything else.
    pub const INTERNAL: &str = "internal";
}

/// An error as JavaScript sees it: `{code, message}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebError {
    /// One of the [`code`] constants.
    pub code: &'static str,
    /// English detail, for logs.
    pub message: String,
}

impl WebError {
    /// An error with `code` and `message`.
    pub fn new(code: &'static str, message: impl Into<String>) -> WebError {
        WebError {
            code,
            message: message.into(),
        }
    }

    /// A malformed argument.
    pub fn invalid(message: impl Into<String>) -> WebError {
        WebError::new(code::INVALID_ARGUMENT, message)
    }

    /// An unexpected failure.
    pub fn internal(message: impl std::fmt::Display) -> WebError {
        WebError::new(code::INTERNAL, message.to_string())
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<LinkError> for WebError {
    fn from(e: LinkError) -> WebError {
        let code = match e {
            LinkError::Truncated => code::LINK_TRUNCATED,
            LinkError::UnsupportedVersion(_) => code::LINK_VERSION,
            LinkError::Corrupt => code::LINK_CORRUPT,
            LinkError::WrongKind => code::LINK_KIND,
        };
        WebError::new(code, e.to_string())
    }
}

impl From<&NetError> for WebError {
    fn from(e: &NetError) -> WebError {
        let code = match e {
            NetError::RelayUnreachable => code::RELAY_UNREACHABLE,
            NetError::Connect { .. } => code::SWARM_ENDED,
            NetError::BadRelay(_) | NetError::RelayRefused(_) => code::LINK_CORRUPT,
            NetError::Bind(_) => code::INTERNAL,
        };
        WebError::new(code, e.to_string())
    }
}

impl From<&StoreError> for WebError {
    fn from(e: &StoreError) -> WebError {
        let code = match e {
            StoreError::Refused => code::ADMISSION_REFUSED,
            StoreError::Unreachable(_) => code::SWARM_ENDED,
            StoreError::Integrity(_) | StoreError::UnsafePath(_) => code::INTEGRITY,
            StoreError::Decrypt(_) => code::DECRYPT,
            StoreError::TooLarge { .. } => code::TOO_LARGE,
            StoreError::NoProvider => code::LINK_CORRUPT,
            _ => code::INTERNAL,
        };
        WebError::new(code, e.to_string())
    }
}

impl From<ValidateError> for WebError {
    fn from(e: ValidateError) -> WebError {
        let code = match e {
            ValidateError::TooLarge(_) => code::TOO_LARGE,
            _ => code::INVALID_MODULE,
        };
        WebError::new(code, e.to_string())
    }
}

/// Parses an endpoint id as printed by `NueeNode.id`.
pub fn parse_id(s: &str) -> Result<EndpointId, WebError> {
    s.trim()
        .parse()
        .map_err(|e| WebError::invalid(format!("invalid endpoint id {s:?}: {e}")))
}

/// The six emoji of an endpoint id.
pub fn fingerprint_of(id: &EndpointId) -> Vec<String> {
    nuee_proto::fingerprint(id)
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

/// SVG markup of a QR code for `text`.
pub fn qr_svg(text: &str) -> Result<String, WebError> {
    let qr = qrcode::QrCode::new(text.as_bytes())
        .map_err(|e| WebError::new(code::TOO_LARGE, format!("cannot encode as a QR code: {e}")))?;
    Ok(qr
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

/// Checks a task module against the task ABI.
pub fn validate(bytes: &[u8]) -> Result<ModuleInfo, WebError> {
    Ok(nuee_proto::validate_module(bytes)?)
}

/// What a pasted link points to, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    /// `job`, `file`, `service` or `site`.
    pub kind: &'static str,
    /// Coordinator, host, or first provider.
    pub peer: EndpointId,
    /// Relay URL of that peer, empty when none.
    pub relay: String,
    /// Ciphertext hash (files and sites).
    pub hash: Option<String>,
    /// Ciphertext size (files and sites).
    pub size: Option<u64>,
    /// Every provider (files and sites).
    pub providers: Vec<EndpointId>,
}

/// Parses `input` (full URL, fragment or `K/PAYLOAD`) into a [`LinkInfo`].
pub fn link_info(input: &str) -> Result<LinkInfo, WebError> {
    let info = match Link::parse(input)? {
        Link::Job(l) => LinkInfo {
            kind: "job",
            peer: l.coordinator,
            relay: l.relay.as_str().to_owned(),
            hash: None,
            size: None,
            providers: Vec::new(),
        },
        Link::Service(l) => LinkInfo {
            kind: "service",
            peer: l.host,
            relay: l.relay.as_str().to_owned(),
            hash: None,
            size: None,
            providers: Vec::new(),
        },
        Link::File(l) => file_info("file", &l)?,
        Link::Site(l) => file_info("site", &l)?,
    };
    Ok(info)
}

fn file_info(kind: &'static str, l: &nuee_proto::FileLink) -> Result<LinkInfo, WebError> {
    let first = l
        .providers
        .as_slice()
        .first()
        .ok_or_else(|| WebError::new(code::LINK_CORRUPT, "the link lists no provider"))?;
    Ok(LinkInfo {
        kind,
        peer: first.id,
        relay: first
            .relay
            .as_ref()
            .map(|r| r.as_str().to_owned())
            .unwrap_or_default(),
        hash: Some(l.hash.to_string()),
        size: Some(l.size),
        providers: l.providers.as_slice().iter().map(|p| p.id).collect(),
    })
}

/// JavaScript name of a task kind.
pub fn kind_name(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Wasm => "wasm",
        TaskKind::Wgsl => "wgsl",
        TaskKind::Embed => "embed",
        TaskKind::Llm => "llm",
        TaskKind::Exec => "exec",
        TaskKind::TrainWasm => "trainWasm",
        TaskKind::TrainExec => "trainExec",
    }
}

/// Task kind from its JavaScript name.
pub fn kind_from_name(name: &str) -> Option<TaskKind> {
    TaskKind::ALL.into_iter().find(|k| kind_name(*k) == name)
}

/// Whether a browser can run `kind` at all (native commands cannot).
pub fn browser_kind(kind: TaskKind) -> bool {
    !matches!(kind, TaskKind::Exec | TaskKind::TrainExec)
}

const FAIL_KINDS: [FailKind; 8] = [
    FailKind::Trap,
    FailKind::Oom,
    FailKind::Timeout,
    FailKind::StackOverflow,
    FailKind::Invalid,
    FailKind::Unsupported,
    FailKind::Cancelled,
    FailKind::Io,
];

/// JavaScript name of a failure class.
pub fn fail_name(kind: FailKind) -> &'static str {
    match kind {
        FailKind::Trap => "trap",
        FailKind::Oom => "oom",
        FailKind::Timeout => "timeout",
        FailKind::StackOverflow => "stack_overflow",
        FailKind::Invalid => "invalid",
        FailKind::Unsupported => "unsupported",
        FailKind::Cancelled => "cancelled",
        FailKind::Io => "io",
    }
}

/// Failure class from its JavaScript name.
pub fn fail_from_name(name: &str) -> Option<FailKind> {
    FAIL_KINDS.into_iter().find(|k| fail_name(*k) == name)
}

/// The `accepted` event of a hosted swarm, without its output bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedEvent {
    /// Job number.
    pub job: u32,
    /// Task index.
    pub task: u32,
    /// Producer, as `NueeNode.id` prints it.
    pub by: String,
    /// BLAKE3 of the output, hex.
    pub hash: String,
    /// Milliseconds from the lease to the result, measured by the hosting page (a worker
    /// cannot shorten it); `null` for an output restored from an earlier run.
    pub ms: Option<f64>,
}

impl AcceptedEvent {
    /// The event for an accepted output.
    pub fn new(
        job: u32,
        task: u32,
        by: &EndpointId,
        hash: &nuee_proto::Hash,
        ms: Option<u64>,
    ) -> AcceptedEvent {
        AcceptedEvent {
            job,
            task,
            by: by.to_string(),
            hash: hash.to_string(),
            ms: ms.map(|m| m as f64),
        }
    }
}

/// JavaScript name of a goodbye reason.
pub fn bye_name(reason: ByeReason) -> &'static str {
    match reason {
        ByeReason::Done => "done",
        ByeReason::Banned => "banned",
        ByeReason::Shutdown => "shutdown",
        ByeReason::Rejected => "rejected",
        ByeReason::Full => "full",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::{BoundedString, JobLink, SecretKey};

    const SMOKE_LINK: &str = "https://maxgfr.github.io/nuee/#/J/AHVEU3DD4KOFECV66VIHWEZOYX4ZKR3WV27L464SIIPOU2IUI3JCYGTIOR2HA4Z2F4XXEZLMMF4S4ZLYMFWXA3DFFZXXEZZPAEBAGBAFAYDQQCIKBMGA2DQPCBWAC4TW";

    const SMOKE_PEER: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";

    fn info_peer_string() -> String {
        SecretKey::from_bytes(&[7u8; 32]).public().to_string()
    }

    fn fixed_job_link() -> String {
        let coordinator = SecretKey::from_bytes(&[7u8; 32]).public();
        let relay = BoundedString::try_from("https://relay.example.org/").expect("relay");
        let secret: [u8; 16] = std::array::from_fn(|i| u8::try_from(i).expect("small") + 1);
        Link::Job(JobLink {
            coordinator,
            relay,
            secret,
        })
        .to_url("https://maxgfr.github.io/nuee/")
    }

    /// The smoke test of the generated package (`web/test/pkg-smoke.test.mjs`) hard-codes
    /// this link and the coordinator id; both must stay in sync with it.
    #[test]
    fn smoke_test_link_is_stable() {
        let link = fixed_job_link();
        assert_eq!(info_peer_string(), SMOKE_PEER);
        assert_eq!(link, SMOKE_LINK);
        let info = link_info(&link).expect("parses");
        assert_eq!(info.kind, "job");
        assert_eq!(info.relay, "https://relay.example.org/");
        assert_eq!(info.peer, SecretKey::from_bytes(&[7u8; 32]).public());
    }

    #[test]
    fn truncated_link_is_reported_as_such() {
        let link = fixed_job_link();
        let cut = &link[..link.len() - 5];
        assert_eq!(
            link_info(cut).expect_err("truncated").code,
            code::LINK_TRUNCATED
        );
    }

    #[test]
    fn names_round_trip() {
        for k in TaskKind::ALL {
            assert_eq!(kind_from_name(kind_name(k)), Some(k));
        }
        for k in FAIL_KINDS {
            assert_eq!(fail_from_name(fail_name(k)), Some(k));
        }
    }

    #[test]
    fn accepted_event_carries_the_measured_time() {
        let by = SecretKey::from_bytes(&[3u8; 32]).public();
        let hash = nuee_proto::Hash::of(b"out");
        let ev = AcceptedEvent::new(1, 2, &by, &hash, Some(1250));
        assert_eq!(ev.ms, Some(1250.0));
        assert_eq!(ev.by, by.to_string());
        assert_eq!(ev.hash, hash.to_string());
        assert_eq!(AcceptedEvent::new(1, 2, &by, &hash, None).ms, None);
    }

    #[test]
    fn qr_is_svg() {
        let svg = qr_svg("x").expect("qr");
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));
    }
}
