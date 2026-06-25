//! Sparse-capability session credentials.
//!
//! AXP's locked spec (§7.1) requires *unforgeable references where possession
//! is authority*.  Because the runtime is **stateful** (it stores live
//! sessions), the correct model is a **sparse capability**: a high-entropy,
//! opaque token is minted per session and stored server-side on the
//! [`Session`](crate::Session).  Knowing the unguessable value *is* the
//! authority — there is no signing key, no HMAC, and no JWT to verify.
//!
//! # Secret handling
//!
//! [`CapToken`] is treated as a secret throughout:
//!
//! - It deliberately does **not** implement `Display`, `PartialEq`, `Eq`,
//!   `Serialize`, or `Deserialize`, so it cannot accidentally be printed,
//!   compared in variable time, or serialized into logs/wire frames.
//! - Its hand-written [`Debug`](std::fmt::Debug) impl **redacts** the value, so
//!   the raw token can never leak into audit logs, panic messages, or error
//!   chains.
//! - The only way to read the raw value is the deliberately-named
//!   [`expose`](CapToken::expose), and the only way to check a presented value
//!   is the constant-time [`verify`](CapToken::verify).

use crate::Error;

/// Number of random bytes drawn from the OS CSPRNG per token.
const TOKEN_BYTES: usize = 32;

/// Human-readable prefix identifying a capability token on the wire.
const TOKEN_PREFIX: &str = "ct_";

/// An unforgeable session credential (a sparse capability).
///
/// Possession of the raw value *is* authority over the associated session.
/// The value is high-entropy random data and is treated as a secret: see the
/// [module docs](self) for the redaction and constant-time guarantees.
///
/// `Clone` is derived so the minted token can be both stored on the session and
/// returned to the caller.  No other common trait is derived on purpose — in
/// particular there is no `Debug`/`Display`/`PartialEq` that could leak or
/// timing-compare the secret.
#[derive(Clone)]
pub struct CapToken(String);

impl CapToken {
    /// Mint a fresh capability token from the operating-system CSPRNG.
    ///
    /// Reads [`TOKEN_BYTES`] bytes of OS entropy, lowercase-hex-encodes them,
    /// and prefixes the result with `"ct_"`.  The resulting token is
    /// `3 + 2 * TOKEN_BYTES` characters long.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Entropy`] if the OS entropy source cannot be read.  The
    /// underlying [`getrandom`] error is not propagated, so no host state leaks
    /// into the error; the call never panics.
    pub fn generate() -> Result<CapToken, Error> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::getrandom(&mut bytes).map_err(|_| Error::Entropy)?;

        // Hand-rolled lowercase hex encoding: two chars per byte, plus the
        // prefix. Avoids pulling in a `hex` dependency for a trivial encode.
        let mut token = String::with_capacity(TOKEN_PREFIX.len() + 2 * TOKEN_BYTES);
        token.push_str(TOKEN_PREFIX);
        for byte in bytes {
            use std::fmt::Write as _;
            // Writing to a String is infallible, but `write!` returns a Result;
            // discard it explicitly rather than unwrapping in library code.
            let _ = write!(token, "{byte:02x}");
        }

        Ok(CapToken(token))
    }

    /// Expose the raw token string.
    ///
    /// The deliberately-explicit name signals that the caller is handling a
    /// secret.  Used by the transport handler to place the freshly minted token
    /// on the wire response back to the client.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison of this token against a `presented` value.
    ///
    /// Returns `true` only if `presented` exactly equals the stored token.  The
    /// comparison runs in time independent of how many leading bytes match, so
    /// an attacker cannot recover the credential byte-by-byte via a timing
    /// side-channel.  A length mismatch short-circuits to `false` (the lengths
    /// are not themselves secret).
    pub fn verify(&self, presented: &str) -> bool {
        let stored = self.0.as_bytes();
        let presented = presented.as_bytes();

        if stored.len() != presented.len() {
            return false;
        }

        // Fold the XOR of every byte pair into an accumulator; it stays zero
        // iff all bytes matched. Touching every byte keeps the running time
        // independent of the position of the first difference.
        let mut acc = 0u8;
        for (a, b) in stored.iter().zip(presented.iter()) {
            acc |= a ^ b;
        }
        acc == 0
    }
}

impl std::fmt::Debug for CapToken {
    /// Redacts the secret so the raw token can never reach logs or panics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CapToken(\"ct_…redacted\")")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected token length: prefix + two hex chars per random byte.
    const EXPECTED_LEN: usize = TOKEN_PREFIX.len() + 2 * TOKEN_BYTES;

    #[test]
    fn generate_produces_prefixed_token_of_expected_length() {
        let token = CapToken::generate().expect("entropy");
        let raw = token.expose();
        assert!(raw.starts_with("ct_"), "expected ct_ prefix, got: {raw}");
        assert_eq!(raw.len(), EXPECTED_LEN, "unexpected token length: {raw}");
        // 3 prefix chars + 64 hex chars.
        assert_eq!(raw.len(), 3 + 64);
        assert!(
            raw[3..].bytes().all(|b| b.is_ascii_hexdigit()),
            "body must be lowercase hex: {raw}"
        );
    }

    #[test]
    fn two_generated_tokens_differ() {
        let a = CapToken::generate().expect("entropy");
        let b = CapToken::generate().expect("entropy");
        assert_ne!(a.expose(), b.expose(), "tokens must be unguessable/unique");
    }

    #[test]
    fn verify_accepts_exact_token() {
        let token = CapToken::generate().expect("entropy");
        let raw = token.expose().to_owned();
        assert!(token.verify(&raw));
    }

    #[test]
    fn verify_rejects_wrong_truncated_and_empty() {
        let token = CapToken::generate().expect("entropy");
        let raw = token.expose().to_owned();

        // Wrong token (same length, different content).
        let mut wrong = String::from("ct_");
        wrong.push_str(&"f".repeat(64));
        assert!(!token.verify(&wrong), "wrong token must be rejected");

        // Truncated token.
        assert!(
            !token.verify(&raw[..raw.len() - 1]),
            "truncated token must be rejected"
        );

        // Empty string.
        assert!(!token.verify(""), "empty string must be rejected");
    }

    #[test]
    fn debug_output_redacts_raw_token() {
        let token = CapToken::generate().expect("entropy");
        let raw = token.expose().to_owned();
        let debug = format!("{token:?}");
        assert!(
            !debug.contains(&raw),
            "Debug output must not contain the raw token: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "Debug output should signal redaction: {debug}"
        );
    }
}
