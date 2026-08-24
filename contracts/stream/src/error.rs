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
    /// The stream has already completed (now >= end_time) and cannot be cancelled.
    StreamAlreadyCompleted = 9,
    /// `total_amount` exceeds the maximum allowed value.
    ///
    /// The vesting calculation multiplies `total_amount` by an elapsed-time
    /// value that can be as large as `u64::MAX`. Capping amounts at `i64::MAX`
    /// guarantees the product never overflows `i128`, because
    /// `i64::MAX * u64::MAX < i128::MAX`.
    AmountTooLarge = 10,
    /// The stream's time window ends entirely in the past.
    ///
    /// `end_time` is before the current ledger timestamp, meaning the stream
    /// would be 100 % vested the moment it is created — effectively an
    /// immediate transfer. Use a token transfer directly instead.
    StreamWindowInPast = 11,
    /// The stream counter has no ids left to assign.
    ///
    /// Ids come from a monotonic `u64` counter that never reuses a value. Once
    /// it reaches `u64::MAX` no further stream can be opened: incrementing
    /// past that point would wrap to zero and hand out ids that already exist
    /// in storage, silently overwriting live streams. Reaching this bound
    /// requires `u64::MAX` successful creations and is not realistic in
    /// practice, but the contract fails closed rather than corrupt existing
    /// records.
    StreamCountExhausted = 12,
    /// A participant address is not valid for the role it was given.
    ///
    /// Returned when `sender` and `recipient` are the same address: such a
    /// stream only locks the sender's own tokens and returns them over time,
    /// which is almost always a swapped or unset argument rather than an
    /// intent.
    ///
    /// Also returned when the stream contract's own address appears as
    /// `sender`, `recipient`, or `token`. As `recipient` the stream would be permanently
    /// unwithdrawable: `withdraw` requires the recipient's authorization and
    /// the contract cannot sign for itself, so the locked tokens would have no
    /// claimant. As `token` it would mean invoking `transfer` on this
    /// contract, which exposes no such entry point. As `sender` a caller could
    /// draw on the contract's own holdings, which back every other stream.
    InvalidParticipant = 13,
}
