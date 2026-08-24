# Changelog

Notable changes to the `stream` contract. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Error codes are part of the public ABI: off-chain consumers match on the
integer, not the variant name. Every change to the set of codes is recorded
here and marked **ABI**, whether or not any reachable behaviour changes with
it.

## [Unreleased]

Nothing has been released yet and no version is tagged; `0.1.0` is still in
development. This changelog starts here, so earlier development history is in
the git log rather than below.

### Added

- **ABI:** `StreamError::StreamCountExhausted`, error code `12`. Stream ids come
  from a monotonic `u64` counter, and `create_stream` previously incremented it
  unchecked. At `u64::MAX` that increment would wrap to zero and the next
  stream would be written over the record already holding id `0`, destroying it
  along with the claim on its locked tokens. Creation now fails with this code
  instead, checked before any tokens move so a rejected call costs the caller
  nothing.

  Reaching the bound takes `u64::MAX` successful creations, so no existing
  caller can observe this in practice; it is a fail-closed guard, not a new
  routine failure mode.

- **ABI:** `StreamError::InvalidParticipant`, error code `13`. `create_stream`
  now rejects this contract's own address as `sender`, `recipient`, or `token`,
  checked before any tokens move. Each role previously failed in its own
  unhelpful way:

  - As `recipient` the call **succeeded**, locking the tokens permanently.
    `withdraw` requires the recipient's authorization and the contract cannot
    sign for itself, so nothing could ever claim them.
  - As `sender` the transfer failed inside the token contract, which returned
    its own `BalanceError`. That code is `10`, the same number as
    `AmountTooLarge`, so the generated client decoded a token-contract failure
    as an unrelated stream error.
  - As `token` the call aborted at the host level with no typed error, because
    this contract exposes no `transfer` entry point.

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
  deliberate.
