//! Status codes returned by host functions (`docs/host-abi.md` section 3).
//!
//! Functions return an `i32`, or an `i64` whose negative values are status codes.

/// Success.
pub const OK: i32 = 0;
/// Bad argument: out-of-bounds buffer, invalid UTF-8, unknown level, ...
pub const ERR_INVALID: i32 = -1;
/// Linked but refused by the instance's grant.
pub const ERR_DENIED: i32 = -2;
/// No such key or object.
pub const ERR_NOT_FOUND: i32 = -3;
/// The handle is not live or belongs to another instance.
pub const ERR_BAD_HANDLE: i32 = -4;
/// Input over the per-call limit or a per-capability size limit.
pub const ERR_TOO_LARGE: i32 = -5;
/// A per-instance budget of this capability is exhausted.
pub const ERR_QUOTA: i32 = -6;
/// The host adapter failed.
pub const ERR_IO: i32 = -7;
/// Orderly end of a stream (`orbit:net/v1` `stream_recv`): the peer closed its sending side.
pub const ERR_EOF: i32 = -8;
/// A waiting call hit its timeout (`orbit:net/v1`).
pub const ERR_TIMEOUT: i32 = -9;
/// The connection was reset, aborted or the pipe is broken (`orbit:net/v1`).
pub const ERR_RESET: i32 = -10;
/// A waiting call was cancelled through its wake source (`orbit:net/v1`).
pub const ERR_CANCELLED: i32 = -11;

/// Symbolic name of a status code, for diagnostics.
#[must_use]
pub fn name(code: i32) -> &'static str {
    match code {
        OK => "OK",
        ERR_INVALID => "ERR_INVALID",
        ERR_DENIED => "ERR_DENIED",
        ERR_NOT_FOUND => "ERR_NOT_FOUND",
        ERR_BAD_HANDLE => "ERR_BAD_HANDLE",
        ERR_TOO_LARGE => "ERR_TOO_LARGE",
        ERR_QUOTA => "ERR_QUOTA",
        ERR_IO => "ERR_IO",
        ERR_EOF => "ERR_EOF",
        ERR_TIMEOUT => "ERR_TIMEOUT",
        ERR_RESET => "ERR_RESET",
        ERR_CANCELLED => "ERR_CANCELLED",
        _ => "UNKNOWN",
    }
}
