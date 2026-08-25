# Security Policy

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

This repository holds fund-moving code. Bugs here can put tokens at risk, so
we ask that you follow responsible disclosure and report privately before any
public discussion.

Report vulnerabilities through the canonical disclosure policy maintained in
the TricklePay documentation repository:

**[https://github.com/Glittersup/tricklepay-docs/security/policy](https://github.com/Glittersup/tricklepay-docs/security/policy)**

That page explains what to include in a report, the expected response timeline,
and how coordinated disclosure is handled. GitHub also surfaces this file in
the "Report a vulnerability" button on the Security tab of this repository.

## Scope

Reports are in scope if they concern code in this repository — the `stream`
contract, its vesting logic, storage layer, or events — and could result in:

- loss or lock-up of user funds,
- unauthorised withdrawal or transfer of tokens,
- bypassing of the cliff or vesting schedule,
- integer overflow or underflow in amount calculations.

## Out of scope

The following are **not** in scope for this policy:

- Bugs in Soroban, the Stellar protocol, or the Stellar network itself —
  report those to the [Stellar Bug Bounty](https://hackerone.com/stellar).
- Issues in downstream repositories (`tricklepay-backend`,
  `tricklepay-frontend`, `tricklepay-docs`).
- Purely theoretical issues with no demonstrated impact path.

## Security model summary

This contract has **no pause, freeze, upgrade, or emergency-stop mechanism**.
There is no privileged admin key. Once deployed, the bytecode is immutable and
every token locked in a stream is exposed to any vulnerability in the deployed
code for the full duration of that stream. The sender's `cancel` is the only
unilateral escape, and it only recovers the unvested remainder.

Full details are in [THREAT_MODEL.md](THREAT_MODEL.md).
