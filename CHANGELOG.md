# Changelog

Notable changes to the `stream` contract. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Error codes are part of the public ABI: off-chain consumers match on the
integer, not the variant name. Every change to the set of codes is recorded
here and marked **ABI**, whether or not any reachable behaviour changes with
it.

Event payloads are part of the public ABI: indexers decode fields by name and
position. Every addition or removal of a field is marked **ABI**.

Entry points (callable functions) are part of the public ABI. Every addition
is marked **ABI**.

## [Unreleased]

Nothing has been released yet and no version is tagged; `0.1.0` is still in
development. All changes below are unreleased and recorded here so downstream
consumers have a single document to track.

### Added

- **ABI:** `create_stream(sender, recipient, token, total_amount, start_time,
  end_time, cliff_time) → u64` — opens a new stream, pulls the full
  `total_amount` from the sender into the contract, and returns the assigned
  stream id. Added 2026-06-17.

- **ABI:** `withdraw(id) → i128` — lets the recipient pull all vested-but-not-yet-
  withdrawn tokens in one call. Returns the amount transferred. Added
  2026-06-17.

- **ABI:** `cancel(id) → i128` — lets the sender stop a stream early. The vested
  portion stays claimable by the recipient; the unvested remainder is refunded
  to the sender. Returns the refund amount. Added 2026-06-17.

- **ABI:** `get_stream(id) → Stream` — returns the full stream record for the
  given id. Added 2026-06-18.

- **ABI:** `withdrawable(id) → i128` — returns the amount the recipient can
  withdraw right now (vested minus already withdrawn). Added 2026-06-18.

- **ABI:** `vested(id) → i128` — returns the total amount vested so far,
  including anything already withdrawn. Added 2026-06-18.

- **ABI:** `status(id) → StreamStatus` — returns the lifecycle state of a stream
  (`Pending`, `Streaming`, `Completed`, or `Cancelled`) at the current ledger
  time. Added 2026-06-18.

- **ABI:** `stream_count() → u64` — returns the number of streams created so
  far; valid ids run from `0` to `stream_count - 1`. Added 2026-06-18.

- **ABI:** `withdraw_amount(id, amount) → i128` — partial withdrawal; lets the
  recipient take a specific amount up to the currently withdrawable balance
  rather than the full available sum. Fails with `InsufficientBalance` (code 8)
  if the requested amount exceeds what is available. Returns the amount
  transferred. Added 2026-06-21.

- **ABI:** `locked(id) → i128` — returns the portion of `total_amount` that has
  not yet vested (i.e. still locked in the contract and not yet claimable by
  the recipient). A cancelled stream always returns `0` because cancellation
  freezes the total at the vested amount. Added 2026-06-21.

- **ABI:** `progress(id) → u32` — returns vesting progress in basis points, from
  `0` (nothing vested) to `10000` (fully vested). Useful for progress
  indicators without fetching the full stream record. Added 2026-07-11.

### Changed

- **ABI:** `Created` event payload extended with three schedule fields:
  `start_time: u64`, `end_time: u64`, and `cliff_time: u64`. Previously the
  data portion carried only `(id, token, total_amount)`; it now carries
  `(id, token, total_amount, start_time, end_time, cliff_time)`. Indexers that
  were recording only the first three data fields must update their decoders to
  handle the additional fields. Changed 2026-07-11.

### Removed

- **ABI:** `StreamError::Unauthorized`, error code `2`, is removed. Nothing in
  the contract ever constructed it. Authorization is enforced with
  `require_auth()`, which panics with a host auth error before the entry point
  body runs, so a caller could never have received code `2` — it advertised a
  failure mode that did not exist. No reachable behaviour changes; clients
  matching on it can drop that branch.

  The remaining codes keep their original values — `StreamNotFound` is still
  `1`, `InvalidTimeRange` still `3`, through `InsufficientBalance` at `8` — so
  existing indexers continue to decode every error they can actually observe.
  Code `2` is retired and will not be reassigned; the gap in the numbering is
  deliberate. Removed 2026-08-08.

### Fixed (non-ABI)

- Instance TTL is now extended on every `create_stream` call. Previously the
  `StreamCount` storage entry could expire on a long-idle contract, making it
  impossible to create new streams. Existing streams and withdrawals were not
  affected. Fixed 2026-08-08.

- **ABI:** `cancel` now returns `StreamAlreadyCompleted` (code `9`) when called
  on a stream whose `end_time` has already passed. Previously the call would
  succeed and issue a zero-refund, leaving the stream in an inconsistent
  cancelled state after full vesting. Callers that relied on cancelling
  completed streams must handle the new error code. Fixed 2026-08-11.

- **ABI:** `create_stream` now validates that `total_amount` does not exceed
  `i64::MAX` (≈ 9.2 × 10¹⁸ stroops). Amounts above this cap are rejected with
  `AmountTooLarge` (code `10`). The bound prevents `i128` overflow in the
  vesting arithmetic, where `total_amount` is multiplied by an elapsed-time
  value that can be as large as `u64::MAX`. Fixed 2026-08-11.

- **ABI:** `create_stream` now rejects a time window that ends entirely in the
  past. If `end_time` is at or before the current ledger timestamp the call
  fails with `StreamWindowInPast` (code `11`). A stream whose end time has
  already passed would be 100 % vested on creation — effectively an immediate
  transfer. Use a token transfer directly instead. Fixed 2026-08-20.

---

### Error code reference

| Code | Variant               | Status   |
|------|-----------------------|----------|
| 1    | `StreamNotFound`      | Active   |
| 2    | *(retired)*           | Retired  |
| 3    | `InvalidTimeRange`    | Active   |
| 4    | `InvalidAmount`       | Active   |
| 5    | `InvalidCliff`        | Active   |
| 6    | `AlreadyCancelled`    | Active   |
| 7    | `NothingToWithdraw`   | Active   |
| 8    | `InsufficientBalance` | Active   |
| 9    | `StreamAlreadyCompleted` | Active |
| 10   | `AmountTooLarge`      | Active   |
| 11   | `StreamWindowInPast`  | Active   |
