# Threat Model

This document describes the security properties and known limitations of the
`stream` contract. Read it before deciding how much value to lock.

## Design goals

The contract is intentionally minimal. It holds tokens on behalf of two
parties and enforces a linear release schedule. No administrator account
exists. No upgrade path is built in. The contract deployed to a given address
is the contract that will run for the lifetime of that address.

## No pause mechanism

**The contract has no pause, freeze, or emergency-stop function.**

There is no owner, admin, or multisig that can halt withdrawals, block a
sender's cancel, or prevent any other operation. Once a stream is created the
only parties that can affect it are the original sender (cancel) and the
original recipient (withdraw).

### Why this matters

Soroban contracts are immutable after deployment. If a bug is discovered in
the vesting logic, the token-transfer path, or any other part of the contract,
there is no mechanism to:

- stop new funds from being exposed to the vulnerability,
- freeze in-flight streams while a fix is prepared, or
- migrate locked tokens to a patched contract.

Every token locked in a stream is therefore exposed to any bug that exists in
the deployed bytecode for the full duration of that stream.

### Consequence for users

Anyone considering a long-duration stream — multi-month vesting schedules,
multi-year grants, subscription arrangements — should treat the contract's
audit status and the amount they are willing to lock as directly linked. A
stream that cannot be paused or migrated is a commitment whose risk profile
does not improve after the fact.

The sender's `cancel` function is the only unilateral escape hatch. It returns
the unvested portion to the sender, but it does not recover tokens that have
already vested to the recipient. If a bug affects the cancel path itself,
neither party has further recourse through the contract.

### Why no pause was added

A pause mechanism requires a privileged account. Introducing one would create
a new attack surface: the key that holds pause authority becomes a high-value
target, and its compromise would let an attacker freeze every stream on the
contract simultaneously. The design trades operational flexibility for the
removal of that privileged-key risk. This is an explicit choice, not an
oversight.

## Immutability

The contract bytecode is fixed at the Wasm hash recorded on-chain at
deployment. There is no `upgrade` entry point. A bug fix requires deploying a
new contract instance; existing streams do not move automatically.

## Authorization model

Every state-changing operation is guarded by Soroban's `require_auth()`. Only
the `sender` may call `cancel`; only the `recipient` may call `withdraw` or
`withdraw_amount`. No other account, including any deployer or admin, holds
any authority over a stream after it is created.

## Out-of-scope risks

The following risks exist but are outside the scope of this contract:

- **Token contract bugs.** The streamed token is an external contract. A
  vulnerability in that contract can affect transfers regardless of stream
  contract correctness.
- **Stellar network-level events.** Ledger upgrades, validator behaviour, and
  protocol changes are outside this contract's control.
- **Key compromise.** If a sender's or recipient's private key is compromised,
  the attacker can cancel or drain the stream as that party.
- **Front-running.** Because Stellar transactions are publicly visible before
  inclusion, a recipient could theoretically race a cancel transaction, though
  the window is narrow and the vesting math caps what can be withdrawn.

## Summary

| Property | Value |
| --- | --- |
| Pause / emergency stop | **None** |
| Admin or owner account | **None** |
| Upgrade path | **None** |
| Per-stream escape hatch | Sender `cancel` (unvested portion only) |
| Contract immutability | Yes — Wasm hash fixed at deployment |
| Bug containment after deployment | Not possible without redeployment |
