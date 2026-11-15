//! Credential scope, sensitive headers and idempotency admission.

use super::{
    Arc, AtomicBool, AuthConfig, BTreeMap, BodyType, ConnectionConfig, CookiePolicy, EngineError,
    ErrorDetail, IdempotencyLocation, JsonObject, Ordering, PreparedRequest, Url, Value,
    same_origin, validate_idempotency_key,
};

pub(super) fn apply_idempotency(
    connection: &ConnectionConfig,
    key: &str,
    query: &mut JsonObject,
    body: &mut JsonObject,
    headers: &mut BTreeMap<String, String>,
) -> Result<(), EngineError> {
    validate_idempotency_key(key)?;
    let name = connection.idempotency.name.trim();
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "idempotency field name must contain 1 to 256 non-control characters",
        )));
    }
    match connection.idempotency.location {
        IdempotencyLocation::Header => {
            if let Some(existing) = headers
                .iter()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
                && existing != key
            {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "idempotency header conflicts with a configured header",
                )));
            }
            insert_header(headers, name, key.to_owned());
        }
        IdempotencyLocation::Query => {
            insert_idempotency_field(query, name, key)?;
        }
        IdempotencyLocation::Body => {
            if matches!(connection.request.body_type, BodyType::None | BodyType::Raw) {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "body idempotency requires json, form_urlencoded, or multipart body",
                )));
            }
            insert_idempotency_field(body, name, key)?;
        }
    }
    Ok(())
}

fn insert_idempotency_field(
    target: &mut JsonObject,
    name: &str,
    key: &str,
) -> Result<(), EngineError> {
    if let Some(existing) = target.get(name)
        && existing.as_str() != Some(key)
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "idempotency field conflicts with a configured parameter",
        )));
    }
    target.insert(name.to_owned(), Value::String(key.to_owned()));
    Ok(())
}

pub(super) fn public_response_url(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Authorization to carry the caller's credentials, scoped to the origin that
/// owns them.
///
/// Cross-origin follow-ups are opt-in, but authorizing the *navigation* is not
/// the same as authorizing the *forwarding of credentials*: a remote service
/// controlling a link, a `Location` header, or a result URL must not be able to
/// steer a bearer token, Basic credentials, an API key, an explicit cookie, or
/// an mTLS identity to an origin of its choosing.
///
/// The authorization is monotone. Once a chain has left the owning origin it
/// stays revoked for every descendant, including one that returns to the owning
/// origin: an `A -> B -> A` sequence would otherwise let B choose which
/// authenticated request the engine sends to A, which is a confused deputy even
/// though B never sees the secret itself.
#[derive(Clone)]
pub(super) struct CredentialScope {
    pub(super) origin: Url,
    /// Shared and write-once. Revocation reaches every holder of a clone,
    /// including a request that was already built — the remote cancellation is
    /// materialized when the job is registered, but may only be sent much
    /// later, after a further hop has revoked the authorization.
    revoked: Arc<AtomicBool>,
}

impl CredentialScope {
    pub(super) fn new(origin: Url) -> Self {
        Self {
            origin,
            revoked: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn allows(&self, target: &Url) -> bool {
        !self.revoked.load(Ordering::Acquire) && same_origin(&self.origin, target)
    }

    /// Narrows the chain for a follow-up aimed at `target`.
    pub(super) fn narrow(&self, target: &Url) {
        if !self.allows(target) {
            self.revoked.store(true, Ordering::Release);
        }
    }

    /// Strips the credentials from `request` unless this scope still authorizes
    /// them for its URL.
    ///
    /// `preserved` names a header that is not a credential and has to survive,
    /// so that dropping it cannot quietly weaken an unrelated guarantee.
    pub(super) fn apply(&self, request: &mut PreparedRequest, preserved: Option<&str>) {
        if self.allows(&request.url) {
            return;
        }
        request.auth = AuthConfig::None;
        request.headers.retain(|name, _| {
            is_transferable_cross_origin_header(name)
                || preserved.is_some_and(|kept| kept.eq_ignore_ascii_case(name))
        });
        // The session's cookies do not cross the origin, but the caller's
        // handle stays in `caller_session`, so the transport still refuses the
        // request if the session has ended.
        request.cookies = CookiePolicy::default();
        request.tls.client_identity_pem = None;
    }
}

/// Name of the header carrying the idempotency key, when the engine adds one.
///
/// The key is generated by the caller and is not a credential, but dropping it
/// on a cross-origin page would silently break a safety property: an idempotency
/// key also switches on retries for non-idempotent methods, so a request that
/// lost the key would still be retried, without the protection the key gives.
/// Keeping it also matches the query and body locations, which are part of the
/// URL or the payload and were never stripped.
pub(super) fn preserved_idempotency_header<'a>(
    connection: &'a ConnectionConfig,
    options: &crate::ExecutionOptions,
) -> Option<&'a str> {
    if options.idempotency_key.is_none()
        || connection.idempotency.location != IdempotencyLocation::Header
    {
        return None;
    }
    let name = connection.idempotency.name.trim();
    // Both conditions matter: the name has to earn the exception by naming an
    // idempotency key, and it must not read as a credential.
    if !names_an_idempotency_key(name) || is_disallowed_idempotency_header(name) {
        return None;
    }
    Some(name)
}

/// True when the engine added an idempotency header and the credential scope
/// removed it from `request`.
pub(super) fn idempotency_header_withheld(
    connection: &ConnectionConfig,
    options: &crate::ExecutionOptions,
    request: &PreparedRequest,
) -> bool {
    if options.idempotency_key.is_none()
        || connection.idempotency.location != IdempotencyLocation::Header
    {
        return false;
    }
    let name = connection.idempotency.name.trim();
    !request
        .headers
        .keys()
        .any(|header| header.eq_ignore_ascii_case(name))
}

/// True when an idempotency header name would smuggle a credential-shaped
/// header past the checks that exist for exactly that.
///
/// The name is caller configured, so the allowlist exception must not become a
/// way to widen the allowlist: naming the idempotency header `Authorization`
/// would otherwise forward an auth header to an origin the remote service
/// chose. The exception is therefore granted only to a name that says it
/// carries an idempotency key — which the default `Idempotency-Key` does, even
/// though the credential classifier flags its `key` component.
pub(crate) fn is_disallowed_idempotency_header(name: &str) -> bool {
    // Trimmed first: `apply_idempotency` trims before inserting the header, so
    // judging the untrimmed name would let `" Authorization "` through and still
    // emit a real `Authorization` header.
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return true;
    }
    if has_credential_compound(&name) {
        return true;
    }
    // Only an exact idempotency word is discounted, and `key` only alongside
    // one. A component that merely begins with it — `IdempotencyToken`,
    // `IdempotencyApiKey` — is judged like any other.
    let marks_idempotency = header_components(&name).any(is_idempotency_word);
    header_components(&name)
        .filter(|component| {
            !is_idempotency_word(component) && !(marks_idempotency && *component == "key")
        })
        .any(is_credential_component)
}

fn is_idempotency_word(component: &str) -> bool {
    matches!(component, "idempotency" | "idempotence" | "idempotent")
}

/// True when the name positively claims to carry an idempotency *key*.
///
/// Required before the cross-origin allowlist exception is granted. Rejecting
/// credential-shaped names is not enough on its own: it would still let an
/// arbitrary header such as `X-Vendor-Code` ride across an origin carrying a
/// caller-controlled value, and `Idempotency-Status` names a status, not a key.
pub(super) fn names_an_idempotency_key(name: &str) -> bool {
    /// Names that are idempotency keys by convention without saying so.
    const ALIASES: [&str; 2] = ["request-id", "x-request-id"];

    let name = name.trim().to_ascii_lowercase();
    if ALIASES.contains(&name.as_str()) {
        return true;
    }
    let components = header_components(&name).collect::<Vec<_>>();
    let glued = ["idempotencykey", "idempotencekey", "idempotentkey"];
    components.iter().any(|component| glued.contains(component))
        || (components.iter().copied().any(is_idempotency_word) && components.contains(&"key"))
}

/// Whole header-name components that read as credentials.
const CREDENTIAL_WORDS: [&str; 15] = [
    "authorization",
    "bearer",
    "cookie",
    "credential",
    "credentials",
    "jwt",
    "key",
    "passcode",
    "passphrase",
    "passwd",
    "password",
    "secret",
    "session",
    "signature",
    "token",
];

/// One-time-code stems, matched at either end of a component.
///
/// These letter sequences do not open or close any ordinary English word, so
/// matching both ends catches `X-OTP`, `X-OTPCode` and `X-VendorOTP` alike
/// without enumerating the words that can sit next to them — and a new spelling
/// nobody anticipated is caught by the same rule.
const ONE_TIME_CODE_STEMS: [&str; 3] = ["hotp", "otp", "totp"];

/// Words that end a component's meaning. Matched as suffixes so a glued name
/// such as `IdempotencyToken` or `X-SessionToken` is still recognised, while a
/// word that merely *starts* with one — `Secretariat` — is not.
///
/// `key` is deliberately absent: it would classify `X-Monkey`.
const CREDENTIAL_SUFFIXES: [&str; 13] = [
    "accesskey",
    "apikey",
    "authorization",
    "credential",
    "jwt",
    "passcode",
    "passphrase",
    "passwd",
    "password",
    "privatekey",
    "secret",
    "signature",
    "token",
];

/// Compound markers spelled across adjacent components, as in `X-Api-Key`, or
/// glued into one, as in `X-CsrfToken`.
///
/// The `*key` entries look redundant next to the generic `key` handling, but
/// they are not: when `key` is its own component the idempotency rule discounts
/// it, and only the pairing with the component before it — `api` + `key` —
/// still says credential.
const CREDENTIAL_COMPOUNDS: [&str; 16] = [
    "accesskey",
    "accesstoken",
    "apikey",
    "authtoken",
    "clientkey",
    "clientsecret",
    "clienttoken",
    "consumerkey",
    "csrftoken",
    "encryptionkey",
    "idtoken",
    "masterkey",
    "privatekey",
    "refreshtoken",
    "secretkey",
    "sessionid",
];

/// Ordinary English words ending in `key`.
///
/// Anything else ending in `key` is treated as a credential. Enumerating the
/// benign side is what makes this robust: a missing entry here over-classifies
/// a header, while a missing entry in a list of credential spellings would let
/// one through — `X-AppKey`, `X-AccountKey`, `X-ServiceKey` and their kin are
/// endless, and each omission is a leak.
///
/// Only ordinary words belong here. A vendor identifier goes in
/// `BENIGN_HEADER_NAMES` instead: exempting a bare component would exempt every
/// header that happens to contain it, including ones nobody has looked at.
const BENIGN_KEY_WORDS: [&str; 7] = [
    "donkey", "hockey", "jockey", "malarkey", "monkey", "turkey", "whiskey",
];

/// Complete header names that end in `key` but identify routing or
/// partitioning, not a secret.
///
/// Matched in full, not by component, so the exemption covers exactly the
/// header it was verified for. Cosmos DB requires both of these on ordinary
/// document operations. Add an entry only with that kind of evidence, and only
/// as a whole name.
const BENIGN_HEADER_NAMES: [&str; 2] = [
    "x-ms-documentdb-partitionkey",
    "x-ms-documentdb-raw-partitionkey",
];

/// Splits a header name into its components.
///
/// Every character that cannot appear inside a word is a separator, not just
/// `-` and `_`: the HTTP token grammar also allows `.`, `!`, `+`, `~` and more,
/// so `X.Token` is a legal name that has to be classified like `X-Token`.
fn header_components(name: &str) -> impl Iterator<Item = &str> {
    name.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|component| !component.is_empty())
}

fn is_credential_component(component: &str) -> bool {
    // A trailing plural is stripped before matching, so `X-Api-Keys` and
    // `X-Access-Tokens` are read exactly like their singular spellings.
    let stem = component.strip_suffix('s').unwrap_or(component);
    if stem.ends_with("key") && !BENIGN_KEY_WORDS.contains(&stem) {
        return true;
    }
    if ONE_TIME_CODE_STEMS
        .iter()
        .any(|code| stem.starts_with(code) || stem.ends_with(code))
    {
        return true;
    }
    if CREDENTIAL_WORDS.contains(&component)
        || CREDENTIAL_WORDS.contains(&stem)
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| component.ends_with(suffix) || stem.ends_with(suffix))
        || component.starts_with("authenticat")
    {
        return true;
    }
    // `auth*` reads as a credential — `authn`, `authz`, `authtoken` — with the
    // `author*` family as the one ordinary exception, which `authoriz*` is not
    // part of despite sharing the prefix.
    component.starts_with("auth")
        && (!component.starts_with("author") || component.starts_with("authoriz"))
}

/// True when a compound marker is spelled out by a run of adjacent components,
/// or ends one of them.
///
/// A trailing plural is stripped as for single components, so `X-SessionIDs`
/// and `X-Session-Ids` read like `X-SessionID`.
fn has_credential_compound(name: &str) -> bool {
    let singular = |text: &str| text.strip_suffix('s').map(str::to_owned);
    let is_marker = |text: &str| {
        CREDENTIAL_COMPOUNDS.contains(&text)
            || singular(text).is_some_and(|stem| CREDENTIAL_COMPOUNDS.contains(&stem.as_str()))
    };
    let components = header_components(name).collect::<Vec<_>>();
    if components.iter().any(|component| {
        let stem = component.strip_suffix('s').unwrap_or(component);
        CREDENTIAL_COMPOUNDS
            .iter()
            .any(|marker| component.ends_with(marker) || stem.ends_with(marker))
    }) {
        return true;
    }
    (0..components.len()).any(|start| {
        let mut joined = String::new();
        components.iter().skip(start).any(|component| {
            joined.push_str(component);
            is_marker(&joined)
        })
    })
}

/// Conservative classification of header names that may carry credentials.
///
/// An exact blacklist cannot cover vendor specific names such as `X-Auth-Token`
/// or `X-Amz-Security-Token`, so the classification is intentionally
/// over-inclusive. It governs which headers are withheld from public results
/// and which inline headers the runtime boundary refuses.
///
/// It deliberately does *not* decide what crosses an origin: forwarding uses
/// `is_transferable_cross_origin_header`, an allowlist, because there a single
/// unrecognised name would be a leak rather than a missed redaction.
pub(crate) fn is_sensitive_header_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if BENIGN_HEADER_NAMES.contains(&name.as_str()) {
        return false;
    }
    has_credential_compound(&name) || header_components(&name).any(is_credential_component)
}

/// Request headers that may follow a request to a *different* origin.
///
/// Forwarding is an allowlist rather than a blocklist on purpose. Classifying
/// credentials by pattern is adequate for redaction, where a miss only reveals
/// a header the caller already sent to that origin, but it is the wrong shape
/// here: one unrecognised vendor header name is enough to hand a secret to an
/// origin the remote service picked. Only headers describing the representation
/// the caller asked for are carried across.
fn is_transferable_cross_origin_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept"
            | "accept-charset"
            | "accept-encoding"
            | "accept-language"
            | "cache-control"
            | "content-type"
            | "if-match"
            | "if-modified-since"
            | "if-none-match"
            | "if-range"
            | "if-unmodified-since"
            | "pragma"
            | "range"
            | "user-agent"
    )
}

pub(super) fn insert_header(
    headers: &mut std::collections::BTreeMap<String, String>,
    name: &str,
    value: String,
) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.insert(existing, value);
    } else {
        headers.insert(name.to_owned(), value);
    }
}
