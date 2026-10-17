//! The parts of an OAuth login that belong to the RFCs rather than to a vendor.
//!
//! PKCE (RFC 7636), the loopback redirect (RFC 8252 §7.3), the token endpoint's
//! responses (RFC 6749 §5), and the device grant (RFC 8628) are the same code
//! whoever the issuer is. They lived twice — once in `keke-auth-codex` and once
//! in `keke-auth-grok`, byte-identical apart from which config struct they
//! read — until a third caller needed them: an MCP server behind OAuth is not a
//! vendor, and it cannot depend on either of those crates.
//!
//! What is deliberately *not* here is anything an issuer decides: the authorize
//! URL's parameters, the token request's body, what a refresh means. Those
//! differ per vendor, and a shared crate that guessed at them would be a
//! configuration surface pretending to be a protocol.

mod browser;
pub mod device;
pub mod jwt;
mod loopback;
mod pkce;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod token;

pub use browser::open_in_browser;
pub use loopback::Loopback;
pub use pkce::Pkce;
pub use pkce::random_token;
