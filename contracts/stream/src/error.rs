use soroban_sdk::contracterror;

/// Errors returned by the stream contract.
///
/// Each variant maps to a stable integer code so that callers and
/// off-chain indexers can match on a value that does not change between
/// builds.
///
/// Authorization failures are deliberately not represented here. Access
/// control is enforced with `require_auth()`, which panics with a host auth
/// error before the entry point body runs, so an unauthorized call never
/// returns one of these codes. Code 2 was an `Unauthorized` variant that
/// nothing ever constructed; it is retired and must not be reused, which is
/// why the codes below skip it rather than close the gap.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum StreamError {
    /// No stream exists for the requested identifier.
    StreamNotFound = 1,
    /// The start time is not strictly before the end time.
    InvalidTimeRange = 3,
    /// The total amount is zero or negative.
    InvalidAmount = 4,
    /// The cliff falls outside the stream's start and end window.
    InvalidCliff = 5,
    /// The stream has already been cancelled.
    AlreadyCancelled = 6,
    /// There is nothing available to withdraw right now.
    NothingToWithdraw = 7,
    /// The requested withdrawal is larger than the available balance.
    InsufficientBalance = 8,
}
