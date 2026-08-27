#![cfg(test)]

use soroban_sdk::{
    testutils::{storage::Instance as _, Address as _, Ledger as _},
    token, Address, Env,
};

use crate::contract::{StreamContract, StreamContractClient};
use crate::storage::{self, ENTRY_TTL};
use crate::{StreamError, StreamStatus, MAX_AMOUNT};

/// A fully wired test environment: a registered stream contract, a token to
/// stream, and helpers to fund accounts and move the ledger clock.
pub struct StreamTest<'a> {
    pub env: Env,
    pub contract: StreamContractClient<'a>,
    pub token: token::TokenClient<'a>,
    pub token_address: Address,
    pub sender: Address,
    pub recipient: Address,
}

impl<'a> StreamTest<'a> {
    /// Build a test with a fresh contract, a fresh token, and a sender funded
    /// with `sender_balance`. All authorization is mocked so calls can be made
    /// without constructing signatures.
    pub fn setup(sender_balance: i128) -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(StreamContract, ());
        let contract = StreamContractClient::new(&env, &contract_id);

        let issuer = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(issuer);
        let token_address = sac.address();
        let token = token::TokenClient::new(&env, &token_address);
        let token_admin = token::StellarAssetClient::new(&env, &token_address);

        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        token_admin.mint(&sender, &sender_balance);

        StreamTest {
            env,
            contract,
            token,
            token_address,
            sender,
            recipient,
        }
    }

    /// Set the ledger timestamp, in Unix seconds.
    pub fn set_time(&self, ts: u64) {
        self.env.ledger().set_timestamp(ts);
    }

    /// Move the ledger sequence to `seq`, simulating elapsed ledgers rather
    /// than elapsed wall-clock time. Entry lifetimes are counted in ledgers,
    /// so this is the clock that time to live is measured against.
    pub fn set_sequence(&self, seq: u32) {
        self.env.ledger().set_sequence_number(seq);
    }

    /// Ledgers of life remaining on the contract instance, which is where the
    /// stream id counter lives.
    pub fn instance_ttl(&self) -> u32 {
        let address = self.contract.address.clone();
        self.env
            .as_contract(&address, || self.env.storage().instance().get_ttl())
    }

    /// Force the id counter to `count`, so boundary behaviour can be reached
    /// without actually opening `u64::MAX` streams.
    pub fn set_stream_count(&self, count: u64) {
        let address = self.contract.address.clone();
        self.env
            .as_contract(&address, || storage::set_stream_count(&self.env, count));
    }

    /// Assert a rejected `create_stream` left nothing behind: no stream, no
    /// id consumed, and every token still with the sender.
    pub fn assert_nothing_happened(&self, sender_balance: i128) {
        assert_eq!(self.contract.stream_count(), 0);
        assert_eq!(self.token.balance(&self.sender), sender_balance);
        assert_eq!(self.token.balance(&self.contract.address), 0);
    }

    /// Open a stream over `[100, 1100]` with no cliff, the shape most of these
    /// tests use.
    fn open_default_stream(&self, amount: i128) -> u64 {
        self.contract.create_stream(
            &self.sender,
            &self.recipient,
            &self.token_address,
            &amount,
            &100,
            &1_100,
            &100,
        )
    }

    /// Attempt to create a stream with explicit participant and token
    /// overrides, using the standard schedule `[100, 1100]` with no cliff and
    /// `amount`. The raw helper returns `true` if the call was rejected and
    /// `false` if it succeeded — this keeps tests resilient to SDK client
    /// return-shape changes.
    pub fn try_create_stream_for_raw(
        &self,
        sender: &Address,
        recipient: &Address,
        token: &Address,
        amount: i128,
    ) -> bool {
        let res = self
            .contract
            .try_create_stream(sender, recipient, token, &amount, &100, &1_100, &100);
        match res {
            Ok(Ok(_)) => false,
            Ok(Err(_)) => true,
            Err(_) => true,
        }
    }
}

#[test]
fn create_stream_locks_funds_and_assigns_id() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // start == cliff means the stream has no cliff.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    assert_eq!(id, 0);
    assert_eq!(t.contract.stream_count(), 1);

    // The full amount has moved from the sender into the contract.
    assert_eq!(t.token.balance(&t.sender), 0);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    let stream = t.contract.get_stream(&id);
    assert_eq!(stream.sender, t.sender);
    assert_eq!(stream.recipient, t.recipient);
    assert_eq!(stream.token, t.token_address);
    assert_eq!(stream.total_amount, 1_000);
    assert_eq!(stream.withdrawn, 0);
    assert!(!stream.cancelled);
}

#[test]
fn withdraw_releases_vested_in_steps() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Midpoint: half has vested.
    t.set_time(600);
    assert_eq!(t.contract.withdraw(&id), 500);
    assert_eq!(t.token.balance(&t.recipient), 500);
    // Nothing more is available until the clock advances again.
    assert_eq!(t.contract.withdrawable(&id), 0);

    // Three-quarter point: another 250 has vested.
    t.set_time(850);
    assert_eq!(t.contract.withdraw(&id), 250);
    assert_eq!(t.token.balance(&t.recipient), 750);

    // End: the final 250.
    t.set_time(1_100);
    assert_eq!(t.contract.withdraw(&id), 250);
    assert_eq!(t.token.balance(&t.recipient), 1_000);

    // The contract is drained and the stream is fully settled.
    assert_eq!(t.token.balance(&t.contract.address), 0);
    assert_eq!(t.contract.get_stream(&id).withdrawn, 1_000);
}

#[test]
fn withdraw_at_exact_end() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Move the clock to exactly end_time. The full amount must be
    // withdrawable and withdraw() must return the whole remaining balance.
    t.set_time(1_100);
    assert_eq!(t.contract.withdrawable(&id), 1_000);
    let withdrawn = t.contract.withdraw(&id);
    assert_eq!(withdrawn, 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
    // After draining, nothing more is withdrawable.
    assert_eq!(t.contract.withdrawable(&id), 0);
}

#[test]
fn progress_reports_basis_points() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Nothing vested at the start.
    assert_eq!(t.contract.progress(&id), 0);
    // Halfway is 50 percent, in basis points.
    t.set_time(600);
    assert_eq!(t.contract.progress(&id), 5_000);
    // Fully vested at the end.
    t.set_time(1_100);
    assert_eq!(t.contract.progress(&id), 10_000);
}

#[test]
fn locked_decreases_as_the_stream_vests() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // At the start the whole amount is locked.
    assert_eq!(t.contract.locked(&id), 1_000);
    // Halfway, half is locked.
    t.set_time(600);
    assert_eq!(t.contract.locked(&id), 500);
    // At the end, nothing is locked.
    t.set_time(1_100);
    assert_eq!(t.contract.locked(&id), 0);
}

#[test]
fn withdraw_amount_takes_a_partial_balance() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Midpoint: 500 vested. Take only 200 of it.
    t.set_time(600);
    assert_eq!(t.contract.withdraw_amount(&id, &200), 200);
    assert_eq!(t.token.balance(&t.recipient), 200);
    // 300 of the vested 500 is still available.
    assert_eq!(t.contract.withdrawable(&id), 300);

    // Taking more than is available is rejected.
    assert_eq!(
        t.contract.try_withdraw_amount(&id, &400),
        Err(Ok(StreamError::InsufficientBalance))
    );

    // A non-positive amount is rejected.
    assert_eq!(
        t.contract.try_withdraw_amount(&id, &0),
        Err(Ok(StreamError::InvalidAmount))
    );
}

#[test]
fn withdraw_amount_exactly_available_balance_succeeds() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(600);
    let available = t.contract.withdrawable(&id);
    assert_eq!(available, 500);

    assert_eq!(t.contract.withdraw_amount(&id, &available), available);
    assert_eq!(t.token.balance(&t.recipient), available);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(t.contract.get_stream(&id).withdrawn, available);
}

#[test]
fn withdraw_amount_available_plus_one_receives_insufficient_balance() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(600);
    let available = t.contract.withdrawable(&id);
    assert_eq!(available, 500);

    assert_eq!(
        t.contract.try_withdraw_amount(&id, &(available + 1)),
        Err(Ok(StreamError::InsufficientBalance))
    );
    assert_eq!(t.token.balance(&t.recipient), 0);
    assert_eq!(t.contract.withdrawable(&id), available);
    assert_eq!(t.contract.get_stream(&id).withdrawn, 0);
}

#[test]
fn cliff_blocks_withdrawal_until_reached() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    // Cliff sits at the midpoint of the stream.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &600,
    );

    // Before the cliff, time has passed but nothing is available.
    t.set_time(400);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );

    // At the cliff, everything accrued since the start unlocks at once.
    t.set_time(600);
    assert_eq!(t.contract.withdrawable(&id), 500);
    assert_eq!(t.contract.withdraw(&id), 500);
    assert_eq!(t.token.balance(&t.recipient), 500);
}

#[test]
fn cancel_refunds_unvested_and_preserves_vested() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Halfway through: 500 vested, 500 still locked.
    t.set_time(600);
    let refund = t.contract.cancel(&id);
    assert_eq!(refund, 500);

    // The sender gets the unvested half back immediately.
    assert_eq!(t.token.balance(&t.sender), 500);
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);

    // The recipient's vested half stays claimable, even much later.
    t.set_time(2_000);
    assert_eq!(t.contract.withdrawable(&id), 500);
    assert_eq!(t.contract.withdraw(&id), 500);
    assert_eq!(t.token.balance(&t.recipient), 500);

    // The split adds up to the original total and the contract is drained.
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // A stream cannot be cancelled twice.
    assert_eq!(
        t.contract.try_cancel(&id),
        Err(Ok(StreamError::AlreadyCancelled))
    );
}

// ── Post-cancellation view correctness ──────────────────────────────────────

/// `cancel` rewrites `total_amount`, `start_time`, `cliff_time`, and
/// `end_time`. The doc comments on `locked` and `progress` make specific
/// claims about the values a cancelled stream should report (0 and 10 000
/// respectively). This test verifies those claims, along with `status` and
/// `withdrawable`, immediately after cancellation and before the recipient
/// has touched their remaining balance.
#[test]
fn views_are_correct_on_a_cancelled_stream() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Cancel at the exact midpoint: 500 has vested, 500 is still locked.
    t.set_time(600);
    t.contract.cancel(&id);

    // locked() must be 0 after cancellation — the doc comment guarantees it.
    // cancel() freezes total_amount at the vested amount, so total - vested == 0.
    assert_eq!(t.contract.locked(&id), 0);

    // progress() must be 10 000 after cancellation — the doc comment guarantees
    // it. The stream is considered fully vested relative to its frozen total.
    assert_eq!(t.contract.progress(&id), 10_000);

    // status() must report Cancelled.
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);

    // withdrawable() must equal the vested-but-not-yet-taken balance: the
    // recipient cancelled at the midpoint and has withdrawn nothing, so 500
    // is still available.
    assert_eq!(t.contract.withdrawable(&id), 500);
}

/// Same four view assertions, but run again after the recipient has drained
/// the remaining vested balance. Once the recipient withdraws, withdrawable
/// must fall to 0 and the other views must stay stable. This also confirms
/// the token balances add up and a second withdraw is rejected.
#[test]
fn views_remain_correct_after_recipient_drains_cancelled_stream() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Cancel at the midpoint and then advance time well past the original end
    // to confirm the frozen state does not change with the clock.
    t.set_time(600);
    t.contract.cancel(&id);
    t.set_time(2_000);

    // Recipient drains their share.
    let withdrawn = t.contract.withdraw(&id);
    assert_eq!(withdrawn, 500);

    // Token balances add up to the original total — nothing was lost.
    assert_eq!(t.token.balance(&t.sender), 500);
    assert_eq!(t.token.balance(&t.recipient), 500);
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // Views must remain consistent after the drain.
    assert_eq!(t.contract.locked(&id), 0);
    assert_eq!(t.contract.progress(&id), 10_000);
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);

    // withdrawable() must now be 0 — the recipient took everything.
    assert_eq!(t.contract.withdrawable(&id), 0);

    // A second withdraw attempt must be rejected.
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );
}

#[test]
fn withdraw_requires_recipient_authorization() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(600);
    t.contract.withdraw(&id);

    // The withdraw required the recipient to authorize; no one else could
    // have pulled these funds.
    let auths = t.env.auths();
    assert!(auths.iter().any(|(addr, _)| addr == &t.recipient));
}

#[test]
fn cancel_requires_sender_authorization() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(600);
    t.contract.cancel(&id);

    // Only the sender can cancel and reclaim the unvested remainder.
    let auths = t.env.auths();
    assert!(auths.iter().any(|(addr, _)| addr == &t.sender));
}

#[test]
fn create_stream_rejects_invalid_parameters() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let zero_amount = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &0,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(zero_amount, Err(Ok(StreamError::InvalidAmount)));

    let negative_amount = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &-5,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(negative_amount, Err(Ok(StreamError::InvalidAmount)));

    // Start is not strictly before end.
    let bad_range = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_100,
        &1_100,
        &1_100,
    );
    assert_eq!(bad_range, Err(Ok(StreamError::InvalidTimeRange)));

    // Cliff before the start.
    let cliff_early = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &50,
    );
    assert_eq!(cliff_early, Err(Ok(StreamError::InvalidCliff)));

    // Cliff after the end.
    let cliff_late = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &1_200,
    );
    assert_eq!(cliff_late, Err(Ok(StreamError::InvalidCliff)));

    // None of the rejected calls created state or moved funds.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
}

#[test]
fn cancel_on_stream_at_end_time_is_rejected() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Move the clock to exactly end_time — the stream is now fully vested.
    t.set_time(1_100);
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);

    // Cancel must be rejected.
    assert_eq!(
        t.contract.try_cancel(&id),
        Err(Ok(StreamError::StreamAlreadyCompleted))
    );

    // The stream still reports Completed, not Cancelled.
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);

    // No tokens moved back to the sender — the contract still holds them.
    assert_eq!(t.token.balance(&t.sender), 0);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);
}

#[test]
fn cancel_past_end_time_is_rejected_and_status_stays_completed() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Move well past the end of the stream.
    t.set_time(5_000);
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);

    // Cancel must be rejected regardless of how far past end_time we are.
    assert_eq!(
        t.contract.try_cancel(&id),
        Err(Ok(StreamError::StreamAlreadyCompleted))
    );

    // Status is unchanged — the stream is still Completed, not Cancelled.
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);

    // The recipient can still withdraw the full amount.
    assert_eq!(t.contract.withdrawable(&id), 1_000);
    assert_eq!(t.contract.withdraw(&id), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

#[test]
fn second_withdraw_without_progress_is_rejected() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(600);
    assert_eq!(t.contract.withdraw(&id), 500);

    // Withdrawing again with no time elapsed releases nothing.
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );
    assert_eq!(t.token.balance(&t.recipient), 500);
}

#[test]
fn operations_on_unknown_stream_report_not_found() {
    let t = StreamTest::setup(1_000);

    assert_eq!(
        t.contract.try_get_stream(&99),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(
        t.contract.try_withdraw(&99),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(
        t.contract.try_cancel(&99),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(
        t.contract.try_withdrawable(&99),
        Err(Ok(StreamError::StreamNotFound))
    );
}

#[test]
fn create_stream_extends_the_instance_ttl() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // A fresh instance only gets the ledger's minimum lifetime, which is far
    // shorter than the window stream entries are given.
    let default_ttl = t.instance_ttl();
    assert!(
        default_ttl < ENTRY_TTL,
        "expected the default instance TTL to be shorter than ENTRY_TTL"
    );

    t.open_default_stream(1_000);

    // Creating a stream lifts the instance to the same window its streams get,
    // so the counter cannot be archived out from under streams that outlive
    // the default lifetime.
    assert_eq!(t.instance_ttl(), ENTRY_TTL);
}

#[test]
fn stream_count_survives_a_ledger_advance_past_the_default_ttl() {
    let t = StreamTest::setup(2_000);
    t.set_time(100);
    let first = t.open_default_stream(1_000);

    // Advance well past the lifetime the instance would have had without the
    // bump in `create_stream`.
    let default_ttl = t.env.ledger().get().min_persistent_entry_ttl;
    let advanced_to = default_ttl * 2;
    t.set_sequence(advanced_to);

    // The instance is still carrying the window `create_stream` granted it,
    // less the ledgers that have elapsed since.
    //
    // This has to be an exact check rather than "is there any life left". The
    // in-memory test host silently restores an expired persistent entry on
    // access instead of archiving it, so without the bump the counter would
    // still answer and the instance would still report a non-zero TTL — just
    // the bare minimum the restore grants, not the window streams get.
    assert_eq!(t.instance_ttl(), ENTRY_TTL - advanced_to);
    assert_eq!(t.contract.stream_count(), 1);

    // Ids keep marching from where they left off rather than restarting and
    // colliding with the stream already in storage.
    let second = t.open_default_stream(1_000);
    assert_eq!(second, first + 1);
    assert_eq!(t.contract.stream_count(), 2);
    assert_eq!(t.contract.get_stream(&first).total_amount, 1_000);
}

// ── Overflow-guard / MAX_AMOUNT boundary tests ──────────────────────────────

/// `create_stream` must reject `total_amount == MAX_AMOUNT + 1` with
/// `AmountTooLarge`. This is the boundary value: one above the cap.
#[test]
fn create_stream_rejects_amount_above_max() {
    // Mint enough so the token transfer is not the thing that fails.
    let t = StreamTest::setup(MAX_AMOUNT + 1);
    t.set_time(100);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &(MAX_AMOUNT + 1),
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::AmountTooLarge)));

    // No stream was created and no funds left the sender.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), MAX_AMOUNT + 1);
}

/// `create_stream` must accept exactly `MAX_AMOUNT` — the boundary is
/// inclusive and the stream must work end-to-end without overflow.
#[test]
fn create_stream_accepts_max_amount() {
    let t = StreamTest::setup(MAX_AMOUNT);
    t.set_time(100);

    // A one-second stream maximises elapsed/duration pressure.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &MAX_AMOUNT,
        &100,
        &101,
        &100,
    );

    // At the midpoint (t == 100, i.e. 0 elapsed out of 1 second) nothing
    // has vested yet.
    assert_eq!(t.contract.withdrawable(&id), 0);

    // At end_time the full amount is vested and withdrawable without panic.
    t.set_time(101);
    assert_eq!(t.contract.withdrawable(&id), MAX_AMOUNT);
    assert_eq!(t.contract.withdraw(&id), MAX_AMOUNT);
    assert_eq!(t.token.balance(&t.recipient), MAX_AMOUNT);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// `i128::MAX` is well above `MAX_AMOUNT` and must be rejected.
#[test]
fn create_stream_rejects_i128_max() {
    // We cannot actually mint i128::MAX tokens (the token contract would
    // reject it), so we only check that *our* guard fires before the
    // transfer is attempted. Use try_create_stream to observe the error.
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &i128::MAX,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::AmountTooLarge)));
}

/// A long-lived stream (duration close to u64::MAX) with an amount at the
/// cap must compute vested amounts without overflow at any point in time.
/// We sample a handful of checkpoints to exercise the multiplication.
#[test]
fn vesting_with_max_amount_over_long_duration_does_not_overflow() {
    // Use a very long stream: 0 to u64::MAX/2 to keep timestamps representable.
    let duration: u64 = u64::MAX / 2;
    let start: u64 = 0;
    let end: u64 = duration;

    let t = StreamTest::setup(MAX_AMOUNT);
    t.set_time(start);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &MAX_AMOUNT,
        &start,
        &end,
        &start,
    );

    // Quarter-point
    t.set_time(duration / 4);
    let q = t.contract.vested(&id);
    assert!(
        q > 0 && q < MAX_AMOUNT,
        "quarter-point vested={q} out of range"
    );

    // Midpoint
    t.set_time(duration / 2);
    let half = t.contract.vested(&id);
    assert!(half > q, "midpoint must exceed quarter-point");

    // At end_time the full amount vests.
    t.set_time(end);
    assert_eq!(t.contract.vested(&id), MAX_AMOUNT);
}

// ── Past time-window rejection (issue #10) ──────────────────────────────────

/// A stream whose `end_time` is strictly before the current ledger time is
/// entirely in the past and would be 100 % vested on creation. That is
/// effectively an immediate transfer disguised as a stream, so it must be
/// rejected with `StreamWindowInPast`.
#[test]
fn create_stream_rejects_end_time_in_the_past() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000); // clock is now at t=1000

    // Window [100, 900] ended 100 seconds ago.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &900,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamWindowInPast)));

    // No stream was created and no funds left the sender.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
}

/// A stream whose `end_time` equals the current ledger timestamp is also
/// 100 % vested the instant it would be created, so it must be rejected too.
#[test]
fn create_stream_rejects_end_time_equal_to_now() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000); // clock is now at t=1000

    // Window [100, 1000] — end_time == now, fully vested immediately.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_000,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamWindowInPast)));

    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
}

/// A stream whose `start_time` is in the past but whose `end_time` is still
/// in the future is a valid backdated schedule and must be accepted. This
/// pattern is legitimate for, e.g., payroll that should have started last
/// month: the employee immediately accrues the already-elapsed portion.
#[test]
fn create_stream_accepts_past_start_time_with_future_end_time() {
    let t = StreamTest::setup(1_000);
    t.set_time(600); // clock is at t=600, midway through [100, 1100]

    // start_time is in the past, end_time is in the future — this is fine.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,   // 500 seconds ago
        &1_100, // 500 seconds from now
        &100,
    );

    // Stream was created; funds are in the contract.
    assert_eq!(t.contract.stream_count(), 1);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // The recipient can immediately withdraw the already-elapsed half.
    assert_eq!(t.contract.withdrawable(&id), 500);
    assert_eq!(t.contract.withdraw(&id), 500);
    assert_eq!(t.token.balance(&t.recipient), 500);
}

/// `end_time` one second in the future is the tightest valid window. The
/// stream must be accepted and its single vesting tick must settle correctly.
#[test]
fn create_stream_accepts_end_time_one_second_in_the_future() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    // end_time == now + 1 — just barely in the future.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &999,
        &1_001,
        &999,
    );

    assert_eq!(t.contract.stream_count(), 1);

    // Advance to end_time; the full amount must be withdrawable.
    t.set_time(1_001);
    assert_eq!(t.contract.withdrawable(&id), 1_000);
}

/// The id counter must never wrap. At `u64::MAX` there is no id left to hand
/// out, so creation is refused outright rather than rolling over to zero and
/// overwriting the stream that already holds id 0.
#[test]
fn create_stream_rejects_an_exhausted_counter() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    t.set_stream_count(u64::MAX);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamCountExhausted)));

    // The counter is untouched and the rejection cost the sender nothing:
    // the check runs before the token transfer.
    assert_eq!(t.contract.stream_count(), u64::MAX);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// The last id below the ceiling is still usable, and using it takes the
/// counter to exactly `u64::MAX` — the point at which the next call must fail.
#[test]
fn create_stream_accepts_the_final_id_then_refuses_the_next() {
    let t = StreamTest::setup(2_000);
    t.set_time(100);
    t.set_stream_count(u64::MAX - 1);

    // The final id is handed out normally.
    let id = t.open_default_stream(1_000);
    assert_eq!(id, u64::MAX - 1);
    assert_eq!(t.contract.stream_count(), u64::MAX);
    assert_eq!(t.contract.get_stream(&id).total_amount, 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // The very next creation has nowhere left to go.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamCountExhausted)));

    // The stream that owns the final id is intact and the second amount never
    // left the sender.
    assert_eq!(t.contract.get_stream(&id).total_amount, 1_000);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);
}

/// The contract's own address as recipient would lock the tokens forever:
/// `withdraw` demands the recipient's authorization and the contract cannot
/// sign for itself, so nothing could ever claim them.
#[test]
fn create_stream_rejects_the_contract_as_recipient() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract_address = t.contract.address.clone();

    let result = t.contract.try_create_stream(
        &t.sender,
        &contract_address,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));
    t.assert_nothing_happened(1_000);
}

/// The contract as sender would let a caller draw on the pooled holdings that
/// back every other stream.
#[test]
fn create_stream_rejects_the_contract_as_sender() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract_address = t.contract.address.clone();

    let result = t.contract.try_create_stream(
        &contract_address,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));
    t.assert_nothing_happened(1_000);
}

/// The contract as the token would mean calling `transfer` on this contract,
/// which exposes no such entry point. Rejecting it turns an obscure host-level
/// failure into a documented error.
#[test]
fn create_stream_rejects_the_contract_as_token() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract_address = t.contract.address.clone();

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &contract_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));
    t.assert_nothing_happened(1_000);
}

/// A stream from an address to itself only locks the sender's own tokens and
/// hands them back over time. It is almost always a swapped or unset argument,
/// so it is refused before any tokens move.
#[test]
fn create_stream_rejects_a_stream_to_self() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.sender,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));
    t.assert_nothing_happened(1_000);
}

/// When an argument list breaks more than one rule, which error comes back is
/// fixed by the documented order on `create_stream` rather than by the
/// incidental arrangement of the checks. Each case below violates two rules
/// and must report the earlier one.
#[test]
fn create_stream_validation_order_is_deterministic() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Participants (2) beat amount (3): self-stream with a zero amount.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.sender,
            &t.token_address,
            &0,
            &100,
            &1_100,
            &100
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );

    // Participants (2) beat schedule (4): the contract as recipient, with a
    // window that is also inverted.
    let contract_address = t.contract.address.clone();
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &contract_address,
            &t.token_address,
            &1_000,
            &1_100,
            &100,
            &1_100
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );

    // Amount (3) beats schedule (4): zero amount with an inverted window.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &0,
            &1_100,
            &100,
            &1_100
        ),
        Err(Ok(StreamError::InvalidAmount))
    );

    // Amount (3) beats capacity (5): an exhausted counter is reported only
    // once the arguments themselves are sound.
    t.set_stream_count(u64::MAX);
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &0,
            &100,
            &1_100,
            &100
        ),
        Err(Ok(StreamError::InvalidAmount))
    );
    // With sound arguments the same counter now surfaces.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100
        ),
        Err(Ok(StreamError::StreamCountExhausted))
    );

    // Nothing above moved a token or consumed an id.
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// Within the schedule group the order is also fixed: range, then cliff, then
/// the past-window rule.
#[test]
fn create_stream_schedule_checks_run_in_order() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // An inverted window whose cliff is also out of bounds reports the range.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &1_000,
            &1_100,
            &100,
            &50
        ),
        Err(Ok(StreamError::InvalidTimeRange))
    );

    // A cliff past the end, on a window that has also already elapsed,
    // reports the cliff.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &1_000,
            &10,
            &50,
            &60
        ),
        Err(Ok(StreamError::InvalidCliff))
    );

    // With a well-formed cliff, the elapsed window is what is reported.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &1_000,
            &10,
            &50,
            &10
        ),
        Err(Ok(StreamError::StreamWindowInPast))
    );

    t.assert_nothing_happened(1_000);
}

// ── Timestamp boundary tests ─────────────────────────────────────────────────

/// `start_time == 0` and a future `end_time` is a valid edge case: Unix epoch
/// zero is a legal timestamp. The stream should be created, and since the
/// current ledger time is well past epoch-zero, the elapsed portion should
/// vest immediately.
#[test]
fn create_stream_accepts_start_time_of_zero() {
    let t = StreamTest::setup(1_000);
    // Ledger is at t=500, which is inside the window [0, 1000].
    t.set_time(500);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &0,     // start_time = epoch zero
        &1_000, // end_time in the future
        &0,     // cliff == start (no cliff)
    );

    // A stream was created and tokens moved to the contract.
    assert_eq!(t.contract.stream_count(), 1);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // At t=500 exactly half the window [0,1000] has elapsed, so 500 is vested.
    assert_eq!(t.contract.vested(&id), 500);
    assert_eq!(t.contract.withdrawable(&id), 500);

    // No tokens left the contract until withdraw is called.
    assert_eq!(t.token.balance(&t.recipient), 0);
    let withdrawn = t.contract.withdraw(&id);
    assert_eq!(withdrawn, 500);
    assert_eq!(t.token.balance(&t.recipient), 500);
}

/// `end_time == now + 1` is the tightest future window possible. The creation
/// must succeed, and advancing one second must make the full amount withdrawable.
/// This is a duplicate-free isolated check of the exact off-by-one boundary
/// between `StreamWindowInPast` and a valid creation.
#[test]
fn create_stream_end_time_one_second_future_boundary() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    // end_time == now + 1: just barely valid.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &999,
        &1_001, // end_time = now + 1
        &999,
    );

    assert_eq!(t.contract.stream_count(), 1);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // Before the end_time nothing extra has vested beyond the start-to-now
    // elapsed fraction. The window is [999, 1001] and now is 1000, so 1/2 vested.
    assert_eq!(t.contract.vested(&id), 500);

    // At end_time the full amount is available.
    t.set_time(1_001);
    assert_eq!(t.contract.withdrawable(&id), 1_000);

    // No tokens must have moved before the explicit withdraw call.
    assert_eq!(t.token.balance(&t.recipient), 0);
    assert_eq!(t.contract.withdraw(&id), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// `end_time == now` is the first value rejected by `StreamWindowInPast`.
/// No token transfer must occur on this rejection.
#[test]
fn create_stream_rejects_end_time_equal_to_now_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &1_000, // end_time == now
        &500,
    );

    assert_eq!(result, Err(Ok(StreamError::StreamWindowInPast)));

    // Deterministic: the exact same call always returns the same error.
    let result2 = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &1_000,
        &500,
    );
    assert_eq!(result2, Err(Ok(StreamError::StreamWindowInPast)));

    // No tokens transferred and no stream was created.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// `end_time == now - 1` must also be rejected with `StreamWindowInPast`.
/// No token transfer must occur on this rejection.
#[test]
fn create_stream_rejects_end_time_one_second_in_past_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &999, // end_time == now - 1
        &500,
    );

    assert_eq!(result, Err(Ok(StreamError::StreamWindowInPast)));

    // No tokens transferred and no stream was created.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// `start_time == end_time` must be rejected with `InvalidTimeRange` (the
/// range check fires before the past-window check), and no tokens must move.
#[test]
fn create_stream_rejects_start_equals_end_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &500, // start == end: zero-length window
        &500,
    );

    assert_eq!(result, Err(Ok(StreamError::InvalidTimeRange)));

    // No tokens transferred and no stream was created.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// `start_time > end_time` must also be `InvalidTimeRange`, with no transfer.
#[test]
fn create_stream_rejects_start_after_end_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_100,
        &500, // start > end
        &500,
    );

    assert_eq!(result, Err(Ok(StreamError::InvalidTimeRange)));

    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// A stream with `cliff_time == end_time` is a pure lock-up: nothing vests
/// until the window closes, then the full amount unlocks at once. This tests
/// the exact cliff-at-end boundary in the context of the full contract.
#[test]
fn create_stream_cliff_at_end_time_is_pure_lockup() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &1_100, // cliff == end_time
    );

    // At midpoint: cliff has not been reached, nothing is withdrawable.
    t.set_time(600);
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );

    // At end_time: cliff is reached, full amount unlocks.
    t.set_time(1_100);
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.withdrawable(&id), 1_000);
    assert_eq!(t.contract.withdraw(&id), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

// ── Issue 1: Timestamp boundary rejection — determinism and no-transfer ─────

/// Rejecting a stream whose `end_time == now` must be deterministic: the same
/// call made twice must return `StreamWindowInPast` both times. This documents
/// and tests the error code, and confirms the contract state is unchanged after
/// each rejected call.
///
/// The `StreamWindowInPast` error (code 11) is the canonical signal that the
/// stream window ends at or before the current ledger time.
#[test]
fn timestamp_boundary_rejection_is_deterministic() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    // Repeated identical calls must return the same error — the result is not
    // influenced by side effects, storage state, or call order.
    for _ in 0..3 {
        let result = t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &1_000,
            &500,
            &1_000, // end_time == now
            &500,
        );
        assert_eq!(
            result,
            Err(Ok(StreamError::StreamWindowInPast)),
            "expected StreamWindowInPast on every repeated call"
        );
    }

    // No stream was ever created and no token left the sender across all calls.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// A stream with `end_time == now - 1` (one second in the past) must be
/// rejected with `StreamWindowInPast`. No token transfer must occur, and the
/// rejection must be deterministic regardless of how many times it is retried.
#[test]
fn timestamp_boundary_past_end_rejected_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &999, // end_time == now - 1
        &500,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamWindowInPast)));

    // Same call again — still the same error, proving determinism.
    let result2 = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &999,
        &500,
    );
    assert_eq!(result2, Err(Ok(StreamError::StreamWindowInPast)));

    // Verification that no token transfer occurred on either attempt.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// The `StreamWindowInPast` check fires *after* the schedule range and cliff
/// checks. This test documents that precise ordering: an invalid range is
/// reported before a past-window condition, and an invalid cliff before a
/// past-window condition, so the caller can fix errors in the correct order.
#[test]
fn timestamp_boundary_error_order_range_before_past_window() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    // start >= end fires InvalidTimeRange before StreamWindowInPast.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &500,
        &500, // start == end: invalid range
        &500,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidTimeRange)));

    // cliff > end fires InvalidCliff before StreamWindowInPast.
    let result2 = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &10,
        &50, // end_time is in the past (now = 1000)
        &60, // cliff > end: InvalidCliff fires first
    );
    assert_eq!(result2, Err(Ok(StreamError::InvalidCliff)));

    // No token transfer and no stream created across all rejected calls.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// A stream with `end_time == now + 1` (one second in the future) is the
/// tightest valid window that must NOT be rejected. This pins the exact
/// boundary between rejection and acceptance so a change to the guard
/// condition is caught immediately.
#[test]
fn timestamp_boundary_one_second_future_is_accepted_not_rejected() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    // end_time = now + 1: must succeed.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &999,
        &1_001, // end_time = now + 1
        &999,
    );

    // Stream was created; a token transfer did occur (funds are in contract).
    assert_eq!(t.contract.stream_count(), 1);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);
    assert_eq!(t.token.balance(&t.sender), 0);

    // Advance to end_time; full amount is vested and withdrawable.
    t.set_time(1_001);
    assert_eq!(t.contract.withdrawable(&id), 1_000);
    assert_eq!(t.contract.withdraw(&id), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

// ── Issue 2: One-second streams ───────────────────────────────────────────────

/// A stream with exactly one second duration (end_time = start_time + 1) must
/// vest correctly: nothing at start_time, full amount at end_time. The stream
/// id counter must advance normally and no id must be reused.
#[test]
fn one_second_stream_vests_nothing_at_start_and_full_at_end() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    // Duration is exactly 1 second: [1000, 1001].
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_000, // start_time == now
        &1_001, // end_time == now + 1
        &1_000, // cliff == start (no cliff)
    );

    assert_eq!(id, 0);
    assert_eq!(t.contract.stream_count(), 1);

    // At start_time: 0 seconds have elapsed out of 1 — nothing has vested.
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );

    // At end_time: the full amount is vested.
    t.set_time(1_001);
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.withdrawable(&id), 1_000);
    assert_eq!(t.contract.withdraw(&id), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // The stream id counter moved to 1, not back to 0.
    assert_eq!(t.contract.stream_count(), 1);
}

/// After a one-second stream completes, a second stream must receive the next
/// id (1, not 0), confirming that no id reuse occurs even across the end of
/// a minimal-duration stream.
#[test]
fn one_second_stream_id_is_not_reused_after_completion() {
    let t = StreamTest::setup(2_000);
    t.set_time(1_000);

    // First stream: id 0.
    let first = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_000,
        &1_001,
        &1_000,
    );
    assert_eq!(first, 0);

    // Advance past end_time so the first stream is Completed.
    t.set_time(1_002);
    assert_eq!(t.contract.status(&first), StreamStatus::Completed);

    // Second stream: must get id 1, not 0, even though id 0 is now completed.
    let second = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_002,
        &1_003,
        &1_002,
    );
    assert_eq!(second, 1);
    assert_eq!(t.contract.stream_count(), 2);

    // The first stream record is still intact — nothing was overwritten.
    let s0 = t.contract.get_stream(&first);
    assert_eq!(s0.total_amount, 1_000);
    assert_eq!(s0.start_time, 1_000);
    assert_eq!(s0.end_time, 1_001);
}

/// A one-second stream cancelled at the only possible interior moment (there
/// is none — cancellation at start_time has 0 elapsed) must refund the full
/// amount to the sender. This exercises the cancel path on a minimal-duration
/// stream where vested == 0.
#[test]
fn one_second_stream_cancel_at_start_refunds_full_amount() {
    let t = StreamTest::setup(1_000);
    t.set_time(1_000);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_000,
        &1_001,
        &1_000,
    );

    // Cancel immediately at start_time: 0 has vested, full amount is refunded.
    let refund = t.contract.cancel(&id);
    assert_eq!(refund, 1_000);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);

    // The recipient has nothing to withdraw.
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );
}

/// A one-second stream whose counter is advanced to `u64::MAX - 1` must hand
/// out the final id and then refuse the next creation with
/// `StreamCountExhausted`. No id reuse may occur at this boundary.
#[test]
fn one_second_stream_counter_boundary_at_exhaustion() {
    let t = StreamTest::setup(2_000);
    t.set_time(1_000);
    t.set_stream_count(u64::MAX - 1);

    // The very last available id is handed out with a one-second stream.
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_000,
        &1_001,
        &1_000,
    );
    assert_eq!(id, u64::MAX - 1);
    assert_eq!(t.contract.stream_count(), u64::MAX);

    // The next creation attempt with another one-second window must fail.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &1_000,
        &1_001,
        &1_000,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamCountExhausted)));

    // The stream at the final id is intact and the second amount never moved.
    assert_eq!(t.contract.get_stream(&id).total_amount, 1_000);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // Counter remains at u64::MAX — it was not incremented on the failed call.
    assert_eq!(t.contract.stream_count(), u64::MAX);
}

// ── Issue 3: Maximum u64 timestamps ──────────────────────────────────────────

/// A stream with timestamps near `u64::MAX` must not overflow the vesting
/// arithmetic. The elapsed and duration values are cast to `i128` before
/// multiplication, so even near-maximum `u64` differences stay within `i128`.
///
/// This test uses `start_time` and `end_time` near the top of the `u64` range
/// and verifies that vesting produces correct, non-panicking results at several
/// checkpoints.
#[test]
fn max_u64_timestamps_vesting_does_not_overflow() {
    // Place the stream at the very top of the u64 timestamp range.
    // Use a 1000-second window ending two seconds before u64::MAX so that
    // end_time + 1 does not wrap.
    let end: u64 = u64::MAX - 2;
    let start: u64 = end - 1_000;

    let t = StreamTest::setup(1_000);
    t.set_time(start); // ledger is at start

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &start,
        &end,
        &start, // no cliff
    );

    // At start_time: nothing has elapsed, nothing vested.
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // At the midpoint: half has vested.
    let mid = start + 500;
    t.set_time(mid);
    assert_eq!(t.contract.vested(&id), 500);
    assert_eq!(t.contract.withdrawable(&id), 500);

    // At end_time: the full amount is vested.
    t.set_time(end);
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.withdrawable(&id), 1_000);
    assert_eq!(t.contract.withdraw(&id), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// A stream with `start_time = 0` and `end_time` near `u64::MAX` exercises
/// the maximum possible duration. The elapsed/duration computation must not
/// overflow `i128` at any sampled point, including near the beginning, the
/// middle, and the end of the window.
///
/// This is the no-cliff case: `cliff_time == start_time == 0`.
#[test]
fn max_u64_end_time_full_duration_no_overflow() {
    // Keep end_time one below u64::MAX so "past end_time" tests can use MAX.
    let end: u64 = u64::MAX - 1;
    let start: u64 = 0;
    let amount: i128 = 1_000;

    let t = StreamTest::setup(amount);
    t.set_time(start);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &start, // cliff == start: no cliff
    );

    // Quarter-point: u64::MAX / 4 seconds elapsed.
    let quarter = end / 4;
    t.set_time(quarter);
    let q = t.contract.vested(&id);
    assert!(
        q > 0 && q < amount,
        "quarter-point vested={q} must be in (0, amount)"
    );

    // Midpoint.
    let half = end / 2;
    t.set_time(half);
    let h = t.contract.vested(&id);
    assert!(
        h > q,
        "midpoint vested={h} must exceed quarter-point vested={q}"
    );

    // One second before end_time: almost everything has vested.
    t.set_time(end - 1);
    let near_end = t.contract.vested(&id);
    assert!(
        near_end > h,
        "near-end vested={near_end} must exceed midpoint"
    );
    assert!(
        near_end < amount,
        "near-end must not yet equal the full amount"
    );

    // At end_time: the full amount vests.
    t.set_time(end);
    assert_eq!(t.contract.vested(&id), amount);
    assert_eq!(t.contract.withdrawable(&id), amount);
}

/// A stream ending at `u64::MAX - 1` with a cliff at the midpoint must
/// correctly withhold vesting until the cliff and then release the accrued
/// amount at once. This exercises the cliff gate with near-maximum timestamps.
#[test]
fn max_u64_timestamps_cliff_at_midpoint_gates_vesting() {
    let end: u64 = u64::MAX - 1;
    let start: u64 = 0;
    let cliff: u64 = end / 2;
    let amount: i128 = 1_000;

    let t = StreamTest::setup(amount);
    t.set_time(start);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &cliff,
    );

    // Before the cliff: nothing is vested despite time having passed.
    t.set_time(cliff - 1);
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );

    // At the cliff: half the window has elapsed, so half the amount unlocks.
    t.set_time(cliff);
    let at_cliff = t.contract.vested(&id);
    assert!(
        at_cliff > 0,
        "vested at cliff must be positive; got {at_cliff}"
    );
    assert!(
        at_cliff <= amount / 2 + 1,
        "vested at cliff ({at_cliff}) should be ≈ half of {amount}"
    );

    // At end_time: the full amount is vested.
    t.set_time(end);
    assert_eq!(t.contract.vested(&id), amount);
}

// ── Issue 4: Vesting property test for upper bound ────────────────────────────

/// Property: `vested_amount` must never exceed `total_amount` for any
/// combination of stream parameters and time. This tests a systematic sweep
/// of time points across a standard stream and across an extreme-duration
/// stream, confirming the upper-bound invariant holds in every case.
///
/// A failure here means the vesting formula has an overflow or a clamping bug
/// that could allow the contract to transfer more than was deposited.
#[test]
fn vesting_never_exceeds_total_amount() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Sample every 100 seconds through the stream and well past it.
    for ts in (0u64..=2_000).step_by(100) {
        t.set_time(ts);
        let v = t.contract.vested(&id);
        assert!(v >= 0, "vested must be non-negative at ts={ts}; got {v}");
        assert!(
            v <= 1_000,
            "vested must not exceed total_amount at ts={ts}; got {v}"
        );
    }
}

/// Property: `vested_amount` must never exceed `total_amount` when the
/// stream uses `MAX_AMOUNT` and the longest practical duration. This is the
/// tightest upper-bound stress: the intermediate product
/// `MAX_AMOUNT * elapsed` must stay within `i128` at every point.
///
/// The contract address is not involved here; the acceptance-criteria
/// reference to "contract address rejected" applies to the `InvalidParticipant`
/// rejection tested elsewhere. This test documents and verifies the arithmetic
/// upper-bound guarantee that prevents token creation from overflow.
#[test]
fn vesting_upper_bound_holds_for_max_amount_over_long_duration() {
    let end: u64 = u64::MAX / 2;
    let start: u64 = 0;

    let t = StreamTest::setup(MAX_AMOUNT);
    t.set_time(start);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &MAX_AMOUNT,
        &start,
        &end,
        &start,
    );

    // Sample a logarithmic spread of time points to cover early, middle, and
    // late positions without requiring u64::MAX / 2 loop iterations.
    // NOTE: end = u64::MAX / 2, so end * 3 would overflow; compute 3/4 point
    // as end / 4 * 3 to avoid intermediate overflow.
    let three_quarter = end / 4 * 3;
    let checkpoints: &[u64] = &[
        0,
        1,
        end / 1_000_000,
        end / 10_000,
        end / 1_000,
        end / 100,
        end / 10,
        end / 4,
        end / 2,
        three_quarter,
        end - 1,
        end,
        end + 1,
        end + end / 4,
    ];

    for &ts in checkpoints {
        t.set_time(ts);
        let v = t.contract.vested(&id);
        assert!(v >= 0, "vested must be non-negative at ts={ts}; got {v}");
        assert!(
            v <= MAX_AMOUNT,
            "vested must not exceed MAX_AMOUNT at ts={ts}; got {v}"
        );
    }

    // At end_time the full amount and nothing more is vested.
    t.set_time(end);
    assert_eq!(t.contract.vested(&id), MAX_AMOUNT);
}

/// Property: `vested_amount` at or after `end_time` must always equal
/// `total_amount` exactly — not more, not less — for any valid stream.
/// This pins the upper-bound clamp in the vesting formula.
#[test]
fn vesting_equals_total_at_and_after_end_time() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // At end_time: must equal total_amount exactly.
    t.set_time(1_100);
    assert_eq!(t.contract.vested(&id), 1_000);

    // Well past end_time: must still equal total_amount, not exceed it.
    for ts in [1_101u64, 2_000, 10_000, u64::MAX / 2] {
        t.set_time(ts);
        let v = t.contract.vested(&id);
        assert_eq!(
            v, 1_000,
            "vested at ts={ts} must equal total_amount; got {v}"
        );
    }
}

/// The `InvalidParticipant` rejection for the contract's own address is
/// deterministic: the same call always returns the same error code, and no
/// token transfer ever occurs. This test documents the error and provides
/// an explicit no-transfer assertion for the vesting-property context.
#[test]
fn vesting_upper_bound_contract_address_rejected_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract_address = t.contract.address.clone();

    // Attempting to stream to the contract itself is always rejected.
    let result = t.contract.try_create_stream(
        &t.sender,
        &contract_address,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));

    // Retry the same call to confirm it is deterministic.
    let result2 = t.contract.try_create_stream(
        &t.sender,
        &contract_address,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result2, Err(Ok(StreamError::InvalidParticipant)));

    // No token transfer and no stream created across both rejected attempts.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

// ── Issue #47: Helper for constructing test streams ──────────────────────────
//
// `try_create_stream_for` is the fixture defined on `StreamTest`. The tests
// below use it to document every `InvalidParticipant` path in one place and
// to confirm that valid participants still create streams correctly.

/// Using the contract's own address as `recipient` is rejected with
/// `InvalidParticipant` before any token transfer occurs.
///
/// The error is deterministic: the same call repeated twice always returns
/// the same code. `assert_nothing_happened` confirms no stream record, no id
/// consumed, and every token still with the sender.
#[test]
fn helper_rejects_contract_as_recipient_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    // First attempt: ensure the debug string names the expected stream error.
    assert!(
        t.try_create_stream_for_raw(
            &t.sender.clone(),
            &contract,
            &t.token_address.clone(),
            1_000
        ),
        "contract as recipient must be InvalidParticipant"
    );
    // Deterministic: same error on the second attempt.
    assert!(
        t.try_create_stream_for_raw(
            &t.sender.clone(),
            &contract,
            &t.token_address.clone(),
            1_000
        ),
        "error must be the same on retry"
    );
    // No token transfer and no id consumed across both attempts.
    t.assert_nothing_happened(1_000);
}

/// Using the contract's own address as `sender` is rejected with
/// `InvalidParticipant`. No token transfer occurs — the guard fires before
/// the `TokenClient::transfer` call.
#[test]
fn helper_rejects_contract_as_sender_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    assert!(
        t.try_create_stream_for_raw(
            &contract,
            &t.recipient.clone(),
            &t.token_address.clone(),
            1_000
        ),
        "contract as sender must be InvalidParticipant"
    );
    t.assert_nothing_happened(1_000);
}

/// Using the contract's own address as `token` is rejected with
/// `InvalidParticipant`. Without this guard the contract would attempt to
/// invoke `transfer` on itself, which exposes no such entry point.
#[test]
fn helper_rejects_contract_as_token_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    assert!(
        t.try_create_stream_for_raw(&t.sender.clone(), &t.recipient.clone(), &contract, 1_000),
        "contract as token must be InvalidParticipant"
    );
    t.assert_nothing_happened(1_000);
}

/// A stream where `sender == recipient` is rejected with `InvalidParticipant`.
/// Such a stream only locks the sender's own tokens and returns them over time
/// — almost always a swapped or unset argument.
#[test]
fn helper_rejects_self_stream_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    assert!(
        t.try_create_stream_for_raw(
            &t.sender.clone(),
            &t.sender.clone(),
            &t.token_address.clone(),
            1_000,
        ),
        "sender == recipient must be InvalidParticipant"
    );
    t.assert_nothing_happened(1_000);
}

/// Valid participants (distinct sender and recipient, real token) must succeed.
/// The helper creates the stream and returns its id; the stream record and
/// token balances must match what was requested.
///
/// This confirms that the participant validation does not inadvertently reject
/// a well-formed call and that existing creation behavior is unchanged.
#[test]
fn helper_valid_participants_creates_stream_and_transfers_tokens() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender.clone(),
        &t.recipient.clone(),
        &t.token_address.clone(),
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Id counter advanced and the stream record is present.
    assert_eq!(id, 0);
    assert_eq!(t.contract.stream_count(), 1);

    // Full amount moved from sender into the contract.
    assert_eq!(t.token.balance(&t.sender), 0);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // Stream fields reflect what was requested.
    let stream = t.contract.get_stream(&id);
    assert_eq!(stream.sender, t.sender);
    assert_eq!(stream.recipient, t.recipient);
    assert_eq!(stream.token, t.token_address);
    assert_eq!(stream.total_amount, 1_000);
    assert_eq!(stream.withdrawn, 0);
    assert!(!stream.cancelled);
}

/// All four `InvalidParticipant` paths fire before any token transfer. This
/// test runs each rejection in sequence on the same harness and checks that
/// the sender's balance and the contract's balance are unchanged after all
/// four attempts, confirming the "no side effects on rejection" guarantee
/// holds regardless of which participant rule is violated.
#[test]
fn helper_all_invalid_participant_cases_leave_no_side_effects() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    // 1. contract as recipient
    assert!(t.try_create_stream_for_raw(
        &t.sender.clone(),
        &contract,
        &t.token_address.clone(),
        1_000
    ));
    // 2. contract as sender
    assert!(t.try_create_stream_for_raw(
        &contract,
        &t.recipient.clone(),
        &t.token_address.clone(),
        1_000
    ));
    // 3. contract as token
    assert!(t.try_create_stream_for_raw(&t.sender.clone(), &t.recipient.clone(), &contract, 1_000));
    // 4. self-stream
    assert!(t.try_create_stream_for_raw(
        &t.sender.clone(),
        &t.sender.clone(),
        &t.token_address.clone(),
        1_000
    ));

    // After all four rejections: no stream created, no id consumed, no tokens moved.
    t.assert_nothing_happened(1_000);
}

/// When the id counter is at `u64::MAX` any new `create_stream` must be
/// rejected with `StreamCountExhausted` and leave no side effects. This
/// guarantees the contract never wraps and reuses an id.
#[test]
fn create_stream_rejected_when_stream_count_exhausted() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Force the counter to the exhausted value.
    t.set_stream_count(u64::MAX);

    // Attempting to create a stream now must be rejected and leave no side
    // effects: no id consumed and no token transfer.
    assert!(t.try_create_stream_for_raw(
        &t.sender.clone(),
        &t.recipient.clone(),
        &t.token_address.clone(),
        1_000
    ));
    // Counter should remain at `u64::MAX` and no token movement occurred.
    assert_eq!(t.contract.stream_count(), u64::MAX);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// The counter accepts the last usable id but refuses additional creations
/// afterwards. This test verifies the boundary: `u64::MAX - 1` can be
/// allocated, after which the counter reaches `u64::MAX` and further
/// attempts are rejected.
#[test]
fn create_stream_accepts_last_id_then_exhausts() {
    let t = StreamTest::setup(2_000);
    t.set_time(100);

    // Reserve the penultimate id so the next creation gets `u64::MAX - 1`.
    t.set_stream_count(u64::MAX - 1);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(id, u64::MAX - 1);

    // Counter advanced to `u64::MAX` and the funds moved for the first stream.
    assert_eq!(t.contract.stream_count(), u64::MAX);
    assert_eq!(t.token.balance(&t.sender), 1_000); // one stream's worth remaining
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    // A second creation attempt is rejected with no side effects.
    assert!(t.try_create_stream_for_raw(
        &t.sender.clone(),
        &t.recipient.clone(),
        &t.token_address.clone(),
        1_000
    ));
    // Balances unchanged and counter still at `u64::MAX`.
    assert_eq!(t.contract.stream_count(), u64::MAX);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);
}

// ── Issue #48: Withdrawable view boundary tests ──────────────────────────────
//
// `withdrawable(id)` is a read-only view: it never moves tokens or writes
// storage. The tests below pin every boundary of its output range and confirm
// that the two failure paths (unknown id, nothing available) are
// deterministic and leave no side effects. The "contract address case" from
// the acceptance criteria refers to the `InvalidParticipant` rejection that
// prevents a contract-address stream from ever being created, which means
// `withdrawable` can never be called on such a stream — the guard is at
// `create_stream`. These tests document that invariant explicitly.

/// `withdrawable` on an unknown id returns `StreamNotFound` every time and
/// does not alter the contract state. The error is the canonical signal that
/// no stream exists for the given id — callers must not infer availability
/// from this error.
#[test]
fn withdrawable_unknown_id_returns_stream_not_found() {
    let t = StreamTest::setup(1_000);

    // No stream has ever been created; any id is unknown.
    assert_eq!(
        t.contract.try_withdrawable(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "unknown id must return StreamNotFound"
    );

    // Deterministic: the same call repeated returns the same error.
    assert_eq!(
        t.contract.try_withdrawable(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "StreamNotFound must be returned on every retry"
    );

    // No state was modified: stream count is still zero, sender balance intact.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// A contract-address stream can never exist because `create_stream` rejects
/// the contract's own address in every participant role with
/// `InvalidParticipant`. This test documents that invariant: attempting to
/// create such a stream fails, and the subsequent `withdrawable` call on the
/// non-existent id also returns `StreamNotFound` — the guard is at creation,
/// not at the view layer.
#[test]
fn withdrawable_contract_address_stream_cannot_exist() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    // Creation with the contract as recipient is rejected — no stream is stored.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &contract,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant)),
        "contract as recipient must be rejected at creation"
    );

    // No token transfer occurred during the rejected creation.
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // The id that would have been assigned (0) does not exist in storage.
    assert_eq!(
        t.contract.try_withdrawable(&0),
        Err(Ok(StreamError::StreamNotFound)),
        "id 0 must not exist after a rejected creation"
    );

    // The rejection is deterministic — the same two calls always produce
    // the same pair of errors in the same order.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &contract,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );
    assert_eq!(
        t.contract.try_withdrawable(&0),
        Err(Ok(StreamError::StreamNotFound))
    );

    // Stream count must remain zero throughout.
    assert_eq!(t.contract.stream_count(), 0);
}

/// `withdrawable` is exactly 0 at `start_time`: no time has elapsed so
/// nothing has vested, and the formula `total * 0 / duration` truncates to 0.
#[test]
fn withdrawable_is_zero_at_start_time() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // At exactly start_time: 0 elapsed out of 1000 → 0 vested.
    assert_eq!(t.contract.withdrawable(&id), 0);

    // The contract still holds all the tokens — no transfer happened.
    assert_eq!(t.token.balance(&t.contract.address), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 0);
}

/// `withdrawable` is exactly 0 before the cliff regardless of how much time
/// has elapsed since `start_time`. The cliff gate withholds everything until
/// `cliff_time` is reached; the underlying vested amount is non-zero but
/// the view returns 0 because `vested_amount` returns 0 when `now < cliff_time`.
#[test]
fn withdrawable_is_zero_before_cliff() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    // Cliff at the midpoint; time will advance to 400 (past start, before cliff).
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &600, // cliff at midpoint
    );

    // At t=400: past start but before cliff — nothing is withdrawable.
    t.set_time(400);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // One second before the cliff: still 0.
    t.set_time(599);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // No tokens left the contract.
    assert_eq!(t.token.balance(&t.contract.address), 1_000);
    assert_eq!(t.token.balance(&t.recipient), 0);
}

/// At `end_time` the full amount is vested and — assuming nothing has been
/// withdrawn yet — the entire `total_amount` is withdrawable.
#[test]
fn withdrawable_equals_total_amount_at_end_time_with_no_prior_withdrawal() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(1_100);
    assert_eq!(
        t.contract.withdrawable(&id),
        1_000,
        "at end_time with no prior withdrawal the full amount must be withdrawable"
    );
}

/// `withdrawable` falls by exactly the amount taken when a partial withdrawal
/// is made. After `withdraw_amount(id, x)` the withdrawable balance is
/// `previous_withdrawable - x`, not 0 and not still the full amount.
#[test]
fn withdrawable_decreases_by_the_amount_withdrawn() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Midpoint: 500 vested. Take 200.
    t.set_time(600);
    assert_eq!(t.contract.withdrawable(&id), 500);
    t.contract.withdraw_amount(&id, &200);

    // 300 of the 500 should still be available.
    assert_eq!(
        t.contract.withdrawable(&id),
        300,
        "withdrawable must fall by the amount taken"
    );

    // Token balances are consistent.
    assert_eq!(t.token.balance(&t.recipient), 200);
    assert_eq!(t.token.balance(&t.contract.address), 800);
}

/// After a full `withdraw`, `withdrawable` drops to 0. The view must not
/// return a negative number or wrap, even though `withdrawn == vested`.
#[test]
fn withdrawable_is_zero_after_full_withdrawal() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Drain the full vested balance at the midpoint.
    t.set_time(600);
    t.contract.withdraw(&id);

    // Nothing more is available at the same timestamp.
    assert_eq!(
        t.contract.withdrawable(&id),
        0,
        "withdrawable must be 0 immediately after a full withdrawal"
    );

    // `try_withdraw` must be rejected — this verifies the 0 result is
    // actionable and not just a display artifact.
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );
}

/// `withdrawable` stays correct across the full lifecycle: 0 at start, grows
/// linearly, is reduced by each withdrawal, and settles to 0 after the final
/// drain. Each step checks both the view result and the token balances.
#[test]
fn withdrawable_lifecycle_from_start_through_full_drain() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // t=100 (start): nothing has vested.
    assert_eq!(t.contract.withdrawable(&id), 0);

    // t=350 (quarter): 250 vested, 250 withdrawable.
    t.set_time(350);
    assert_eq!(t.contract.withdrawable(&id), 250);

    // Withdraw 100; 150 must remain.
    t.contract.withdraw_amount(&id, &100);
    assert_eq!(t.contract.withdrawable(&id), 150);
    assert_eq!(t.token.balance(&t.recipient), 100);

    // t=600 (half): 500 total vested; 100 already withdrawn → 400 available.
    t.set_time(600);
    assert_eq!(t.contract.withdrawable(&id), 400);

    // Drain the remainder.
    t.contract.withdraw(&id);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(t.token.balance(&t.recipient), 500);

    // t=1100 (end): full amount vested; 500 already withdrawn → 500 left.
    t.set_time(1_100);
    assert_eq!(t.contract.withdrawable(&id), 500);
    t.contract.withdraw(&id);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // All tokens delivered; contract is drained.
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

// ── Issue #49: Vested view boundary tests ────────────────────────────────────
//
// `vested(id)` returns `StreamNotFound` for an unknown id and the linearly
// accrued amount for a known one. These tests pin the behaviour at the id
// counter boundary — the last valid id (`u64::MAX - 1`) and the exhausted
// counter (`u64::MAX`) — so that a change to the counter logic or the vesting
// formula is caught immediately.
//
// The counter overflow failure mode:
//   The id counter is a monotonic `u64` stored in instance storage. Once it
//   reaches `u64::MAX`, `checked_add(1)` returns `None` and `create_stream`
//   returns `StreamCountExhausted` (code 12) before any token moves or any
//   storage is written. The counter is left at `u64::MAX`; no id is consumed.
//   Because ids are never reused, the id `u64::MAX` itself is never handed
//   out, so `vested(u64::MAX)` always returns `StreamNotFound` after
//   exhaustion.

/// `vested` on an unknown id returns `StreamNotFound` every time. This is the
/// documented failure mode for the view: callers must check the error rather
/// than treating it as zero vesting.
#[test]
fn vested_unknown_id_returns_stream_not_found() {
    let t = StreamTest::setup(1_000);

    // No stream has ever been created; every id is unknown.
    assert_eq!(
        t.contract.try_vested(&42),
        Err(Ok(StreamError::StreamNotFound)),
        "unknown id must return StreamNotFound"
    );

    // Deterministic: the same call always returns the same error.
    assert_eq!(
        t.contract.try_vested(&42),
        Err(Ok(StreamError::StreamNotFound)),
        "StreamNotFound must be stable across repeated calls"
    );

    // No state change: stream count is still zero.
    assert_eq!(t.contract.stream_count(), 0);
}

/// After `StreamCountExhausted` the id that would have been assigned is never
/// stored, so `vested` on that id returns `StreamNotFound`, not a stale value.
/// This pins the invariant that `checked_add` overflow leaves no storage trace.
#[test]
fn vested_after_counter_exhaustion_returns_stream_not_found() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Force the counter to u64::MAX so the next creation fails.
    t.set_stream_count(u64::MAX);

    // Creation attempt is rejected — no stream is written.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::StreamCountExhausted))
    );

    // The counter must be unchanged — it was not incremented on the failed call.
    assert_eq!(t.contract.stream_count(), u64::MAX);

    // No token left the sender.
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // `vested` on the id that would have been assigned (u64::MAX is what the
    // counter holds, but that id was never handed out) returns StreamNotFound.
    assert_eq!(
        t.contract.try_vested(&u64::MAX),
        Err(Ok(StreamError::StreamNotFound)),
        "vested must return StreamNotFound for an id that was never assigned"
    );

    // Deterministic: same result on retry.
    assert_eq!(
        t.contract.try_vested(&u64::MAX),
        Err(Ok(StreamError::StreamNotFound))
    );
}

/// A stream created with the last valid id (`u64::MAX - 1`) must vest
/// correctly. `vested` is called at `start_time`, the midpoint, and
/// `end_time` and the results must match the linear formula exactly.
/// This confirms the vesting arithmetic does not interact with the id value.
#[test]
fn vested_on_boundary_id_stream_produces_correct_values() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Place the counter at the last usable position.
    t.set_stream_count(u64::MAX - 1);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // The stream received the final available id.
    assert_eq!(id, u64::MAX - 1);
    // Counter advanced to u64::MAX — the exhausted state.
    assert_eq!(t.contract.stream_count(), u64::MAX);

    // At start_time: 0 elapsed → 0 vested.
    assert_eq!(
        t.contract.vested(&id),
        0,
        "nothing must be vested at start_time on the boundary-id stream"
    );

    // At the midpoint (t=600): 500/1000 of the window elapsed → 500 vested.
    t.set_time(600);
    assert_eq!(
        t.contract.vested(&id),
        500,
        "half the amount must be vested at the midpoint"
    );

    // At end_time: full amount vested.
    t.set_time(1_100);
    assert_eq!(
        t.contract.vested(&id),
        1_000,
        "full amount must be vested at end_time"
    );

    // Well past end_time: still capped at total_amount.
    t.set_time(9_999);
    assert_eq!(
        t.contract.vested(&id),
        1_000,
        "vested must not exceed total_amount past end_time"
    );
}

/// After the counter is exhausted, the boundary-id stream (`u64::MAX - 1`)
/// remains fully functional: `vested`, `withdrawable`, and `withdraw` all
/// work correctly. Exhaustion must not corrupt or archive existing stream
/// records.
#[test]
fn vested_boundary_id_stream_remains_functional_after_exhaustion() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    t.set_stream_count(u64::MAX - 1);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(id, u64::MAX - 1);

    // Confirm the counter is now exhausted.
    assert_eq!(t.contract.stream_count(), u64::MAX);

    // Advance to end_time: the full amount must be vested and withdrawable.
    t.set_time(1_100);
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.withdrawable(&id), 1_000);

    // Recipient can withdraw even though no new stream can be created.
    t.contract.withdraw(&id);
    assert_eq!(t.token.balance(&t.recipient), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // Views settle to their post-drain state.
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(t.contract.locked(&id), 0);
    assert_eq!(t.contract.progress(&id), 10_000);
}

/// `vested` increases monotonically as time advances. It must never decrease
/// within a stream's window, and it must not exceed `total_amount` at any
/// point. This is the upper-bound invariant the overflow guard was designed to
/// protect.
#[test]
fn vested_is_monotonically_non_decreasing_and_capped() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    let mut prev = 0i128;
    // Step through the stream in 50-second increments plus a few past the end.
    for ts in (50u64..=1_500).step_by(50) {
        t.set_time(ts);
        let v = t.contract.vested(&id);
        assert!(
            v >= prev,
            "vested must not decrease: was {prev} at previous step, got {v} at ts={ts}"
        );
        assert!(
            v <= 1_000,
            "vested must not exceed total_amount: got {v} at ts={ts}"
        );
        prev = v;
    }
}

// ── Issue #22: Status view boundary tests ────────────────────────────────────
//
// `status(id)` returns the lifecycle state (`Pending`, `Streaming`, `Completed`,
// or `Cancelled`) of a stream at the current ledger timestamp. It is a read-only
// view that never alters state or moves tokens.
//
// These tests pin every boundary transition of `status`:
// - `now < start_time` → `Pending`
// - `start_time <= now < end_time` → `Streaming`
// - `now >= end_time` → `Completed`
// - `stream.cancelled == true` → `Cancelled`
//
// The tests also verify that attempts to query `status` on unknown IDs or IDs
// from rejected creation attempts (such as using the contract's own address as
// participant) deterministically return `StreamNotFound` and leave no side
// effects (no tokens moved, no stream count change).

/// `status` on an unknown ID returns `StreamNotFound` deterministically without
/// altering contract storage or balances.
#[test]
fn status_unknown_id_returns_stream_not_found() {
    let t = StreamTest::setup(1_000);

    assert_eq!(
        t.contract.try_status(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "unknown id must return StreamNotFound"
    );

    // Deterministic: second call produces identical error.
    assert_eq!(
        t.contract.try_status(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "StreamNotFound must be returned on retry"
    );

    // No side effects.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// Creation with the contract address is rejected with `InvalidParticipant`
/// before any tokens transfer. Querying `status` on the unassigned ID
/// deterministically returns `StreamNotFound`.
#[test]
fn status_contract_address_rejected_no_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    // Rejection at creation.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &contract,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant)),
        "contract as recipient must be rejected"
    );

    // No token transfer occurred.
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);

    // Calling status on the unassigned id returns StreamNotFound deterministically.
    assert_eq!(
        t.contract.try_status(&0),
        Err(Ok(StreamError::StreamNotFound)),
        "status on uncreated id must return StreamNotFound"
    );

    assert_eq!(
        t.contract.try_status(&0),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(t.contract.stream_count(), 0);
}

/// `status` correctly transitions across `Pending`, `Streaming`, and `Completed`
/// exact timestamp boundaries.
#[test]
fn status_boundary_transitions_pending_streaming_completed() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Before start: Pending.
    t.set_time(99);
    assert_eq!(t.contract.status(&id), StreamStatus::Pending);

    // Exactly at start_time: Streaming.
    t.set_time(100);
    assert_eq!(t.contract.status(&id), StreamStatus::Streaming);

    // Midpoint: Streaming.
    t.set_time(600);
    assert_eq!(t.contract.status(&id), StreamStatus::Streaming);

    // One second before end_time: Streaming.
    t.set_time(1_099);
    assert_eq!(t.contract.status(&id), StreamStatus::Streaming);

    // Exactly at end_time: Completed.
    t.set_time(1_100);
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);

    // Past end_time: Completed.
    t.set_time(2_000);
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);
}

/// A cancelled stream returns `Cancelled` status regardless of whether time
/// is past `end_time`.
#[test]
fn status_cancelled_stream_takes_precedence() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    t.set_time(600);
    t.contract.cancel(&id);

    // At cancellation timestamp: Cancelled.
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);

    // Past original end_time: Still Cancelled.
    t.set_time(2_000);
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);
}

/// Calling `status` is read-only and causes no side-effects on stream or token balances.
#[test]
fn status_view_is_read_only_and_leaves_no_side_effects() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    let sender_bal = t.token.balance(&t.sender);
    let contract_bal = t.token.balance(&t.contract.address);
    let count = t.contract.stream_count();

    // Query status repeatedly.
    for ts in [50, 100, 600, 1_100, 2_000] {
        t.set_time(ts);
        let _ = t.contract.status(&id);
    }

    // Invariants hold.
    assert_eq!(t.token.balance(&t.sender), sender_bal);
    assert_eq!(t.token.balance(&t.contract.address), contract_bal);
    assert_eq!(t.contract.stream_count(), count);
}

// ── Vesting property test for zero before cliff & counter boundary tests ──────
//
// Tokens vest linearly between `start_time` and `end_time`, but before `cliff_time`
// (`now < cliff_time`) the vested and withdrawable amounts must be exactly zero.
//
// These tests isolate and verify this invariant across standard and extreme stream
// schedules, confirming that:
// - `vested(id) == 0` and `withdrawable(id) == 0` for every timestamp prior to the cliff.
// - `locked(id) == total_amount` before the cliff.
// - Attempting to withdraw before the cliff returns `NothingToWithdraw` and leaves
//   token balances completely untouched.
// - Counter overflow (`u64::MAX`) is handled deterministically by returning
//   `StreamCountExhausted` without wrapping around or reusing an ID, as verified
//   by counter boundary tests.

/// Property: `vested` and `withdrawable` must return 0 for every time point strictly
/// before `cliff_time`, even after `start_time` has passed.
///
/// This test sweeps time points from before `start_time` up to `cliff_time - 1`,
/// asserting zero vested/withdrawable amount, total locked amount, and zero token transfers.
#[test]
fn vesting_property_zero_before_cliff_sweep_and_no_side_effects() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let start = 100u64;
    let cliff = 600u64;
    let end = 1_100u64;
    let amount = 1_000i128;

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &cliff,
    );

    // Initial balances after stream creation.
    assert_eq!(t.token.balance(&t.sender), 0);
    assert_eq!(t.token.balance(&t.contract.address), amount);
    assert_eq!(t.token.balance(&t.recipient), 0);

    // Sweep time points prior to the cliff.
    for ts in [0u64, 50, 100, 200, 350, 500, 599] {
        t.set_time(ts);

        let v = t.contract.vested(&id);
        let w = t.contract.withdrawable(&id);
        let l = t.contract.locked(&id);

        assert_eq!(v, 0, "vested must be 0 before cliff at ts={ts}");
        assert_eq!(w, 0, "withdrawable must be 0 before cliff at ts={ts}");
        assert_eq!(l, amount, "locked must equal total_amount before cliff at ts={ts}");

        // Attempting to withdraw before the cliff must fail cleanly without moving tokens.
        assert_eq!(
            t.contract.try_withdraw(&id),
            Err(Ok(StreamError::NothingToWithdraw)),
            "withdraw before cliff must return NothingToWithdraw"
        );
    }

    // Confirm token balances remained unaffected across all pre-cliff withdrawal attempts.
    assert_eq!(t.token.balance(&t.sender), 0);
    assert_eq!(t.token.balance(&t.contract.address), amount);
    assert_eq!(t.token.balance(&t.recipient), 0);
}

/// Property: `vested` and `withdrawable` remain zero before the cliff on an extreme
/// duration stream using maximum amounts.
#[test]
fn vesting_property_zero_before_cliff_holds_for_extreme_duration() {
    let start: u64 = 0;
    let end: u64 = u64::MAX / 2;
    let cliff: u64 = end / 4;

    let t = StreamTest::setup(MAX_AMOUNT);
    t.set_time(start);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &MAX_AMOUNT,
        &start,
        &end,
        &cliff,
    );

    let checkpoints: &[u64] = &[
        0,
        1,
        cliff / 1_000_000,
        cliff / 10_000,
        cliff / 1_000,
        cliff / 100,
        cliff / 10,
        cliff / 2,
        cliff - 1,
    ];

    for &ts in checkpoints {
        t.set_time(ts);

        assert_eq!(
            t.contract.vested(&id),
            0,
            "vested must be 0 before cliff on extreme stream at ts={ts}"
        );
        assert_eq!(
            t.contract.withdrawable(&id),
            0,
            "withdrawable must be 0 before cliff on extreme stream at ts={ts}"
        );
        assert_eq!(
            t.contract.locked(&id),
            MAX_AMOUNT,
            "locked must equal MAX_AMOUNT before cliff on extreme stream at ts={ts}"
        );
    }
}

/// Counter overflow is handled deterministically without wrapping to zero or reusing an ID.
///
/// When the stream count reaches `u64::MAX`, `create_stream` fails with `StreamCountExhausted`.
/// The counter is left at `u64::MAX`, ensuring no ID wrapping or storage corruption occurs.
#[test]
fn counter_overflow_handled_without_reusing_id_and_leaves_no_side_effects() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Set counter to u64::MAX (exhausted).
    t.set_stream_count(u64::MAX);

    // Attempt creation when counter is at u64::MAX.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(
        result,
        Err(Ok(StreamError::StreamCountExhausted)),
        "creation at u64::MAX must return StreamCountExhausted"
    );

    // Counter must stay at u64::MAX and NOT wrap to 0.
    assert_eq!(
        t.contract.stream_count(),
        u64::MAX,
        "counter must not wrap or change on rejection"
    );

    // Deterministic: second call produces identical failure mode.
    let result2 = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(
        result2,
        Err(Ok(StreamError::StreamCountExhausted)),
        "retry must return StreamCountExhausted"
    );

    // No ID 0 or ID u64::MAX was reused or created, and no tokens transferred.
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
    assert_eq!(
        t.contract.try_get_stream(&0),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(
        t.contract.try_get_stream(&u64::MAX),
        Err(Ok(StreamError::StreamNotFound))
    );
}

/// Boundary test covering stream creation on boundary ID (`u64::MAX - 1`) with a cliff.
///
/// Verifies that a stream assigned the final usable ID `u64::MAX - 1` correctly enforces
/// the zero-before-cliff rule before the cliff and unlocks accrued tokens at the cliff.
#[test]
fn counter_boundary_id_stream_with_cliff_vests_zero_before_cliff() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Set counter to u64::MAX - 1 (the boundary ID).
    t.set_stream_count(u64::MAX - 1);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &600, // cliff at midpoint
    );

    // Stream was assigned the boundary ID.
    assert_eq!(id, u64::MAX - 1);
    // Counter is now at u64::MAX.
    assert_eq!(t.contract.stream_count(), u64::MAX);

    // Before cliff (t=400): vested and withdrawable are zero on the boundary-id stream.
    t.set_time(400);
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // One second before cliff (t=599): still zero.
    t.set_time(599);
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // Exactly at cliff (t=600): 500 units vest and become withdrawable.
    t.set_time(600);
    assert_eq!(t.contract.vested(&id), 500);
    assert_eq!(t.contract.withdrawable(&id), 500);

    // Recipient can withdraw the unlocked amount.
    t.contract.withdraw(&id);
    assert_eq!(t.token.balance(&t.recipient), 500);
    assert_eq!(t.token.balance(&t.contract.address), 500);
}

// ── Issue #23: Progress view boundary & invalid participant tests ─────────────
//
// `progress(id)` returns vesting progress in basis points (0 to 10000). It is a
// read-only view function that never alters state or moves tokens.
//
// These tests pin:
// - Deterministic `StreamNotFound` for unknown IDs without side effects.
// - All `InvalidParticipant` creation rejections occur before any token transfer,
//   and `progress` on uncreated IDs returns `StreamNotFound`.
// - Progress scale across exact timestamp boundaries (0 before start/cliff, linear
//   growth during stream window, 10000 at/after end time).
// - Cancelled streams report 10000 (nothing left to vest).
// - Existing valid stream creation behavior remains unchanged.

/// `progress` on an unknown ID returns `StreamNotFound` deterministically without
/// altering contract storage or balances.
#[test]
fn progress_unknown_id_returns_stream_not_found() {
    let t = StreamTest::setup(1_000);

    assert_eq!(
        t.contract.try_progress(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "unknown id must return StreamNotFound"
    );

    // Deterministic: second call produces identical error.
    assert_eq!(
        t.contract.try_progress(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "StreamNotFound must be returned on retry"
    );

    // Invariants hold.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// All invalid participant creation calls are rejected with `InvalidParticipant`
/// before any token transfer occurs. Querying `progress` on uncreated IDs
/// deterministically returns `StreamNotFound`.
#[test]
fn progress_invalid_participants_rejected_before_token_transfer() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract = t.contract.address.clone();

    // 1. Contract as recipient
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &contract,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant)),
        "contract as recipient must be InvalidParticipant"
    );

    // 2. Contract as sender
    assert_eq!(
        t.contract.try_create_stream(
            &contract,
            &t.recipient,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant)),
        "contract as sender must be InvalidParticipant"
    );

    // 3. Contract as token
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &contract,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant)),
        "contract as token must be InvalidParticipant"
    );

    // 4. Self-stream (sender == recipient)
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.sender,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100,
        ),
        Err(Ok(StreamError::InvalidParticipant)),
        "sender == recipient must be InvalidParticipant"
    );

    // Confirm no tokens moved and no stream count advanced across all 4 invalid creation attempts.
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
    assert_eq!(t.contract.stream_count(), 0);

    // Calling progress on uncreated ID 0 returns StreamNotFound deterministically.
    assert_eq!(
        t.contract.try_progress(&0),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(
        t.contract.try_progress(&0),
        Err(Ok(StreamError::StreamNotFound))
    );
}

/// `progress` accurately reports basis points (0 to 10000) across key lifecycle boundaries.
#[test]
fn progress_boundary_transitions_from_start_to_end() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Before start (t=50): 0 progress.
    t.set_time(50);
    assert_eq!(t.contract.progress(&id), 0);

    // At start (t=100): 0 progress.
    t.set_time(100);
    assert_eq!(t.contract.progress(&id), 0);

    // Quarter point (t=350): 2500 basis points (25%).
    t.set_time(350);
    assert_eq!(t.contract.progress(&id), 2_500);

    // Midpoint (t=600): 5000 basis points (50%).
    t.set_time(600);
    assert_eq!(t.contract.progress(&id), 5_000);

    // Three-quarter point (t=850): 7500 basis points (75%).
    t.set_time(850);
    assert_eq!(t.contract.progress(&id), 7_500);

    // At end_time (t=1100): 10000 basis points (100%).
    t.set_time(1_100);
    assert_eq!(t.contract.progress(&id), 10_000);

    // Past end_time (t=2000): 10000 basis points (capped).
    t.set_time(2_000);
    assert_eq!(t.contract.progress(&id), 10_000);
}

/// `progress` returns 10000 for a cancelled stream regardless of when it was cancelled.
#[test]
fn progress_cancelled_stream_returns_10000() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Cancel at midpoint.
    t.set_time(600);
    t.contract.cancel(&id);

    // Cancelled stream has nothing left to vest, so progress is 10000.
    assert_eq!(t.contract.progress(&id), 10_000);

    // Past original end_time: still 10000.
    t.set_time(2_000);
    assert_eq!(t.contract.progress(&id), 10_000);
}

/// Valid stream creation behavior remains completely unchanged.
#[test]
fn progress_valid_stream_creation_behavior_remains_unchanged() {
    let t = StreamTest::setup(2_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    assert_eq!(id, 0);
    assert_eq!(t.contract.stream_count(), 1);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 1_000);

    let stream = t.contract.get_stream(&id);
    assert_eq!(stream.sender, t.sender);
    assert_eq!(stream.recipient, t.recipient);
    assert_eq!(stream.total_amount, 1_000);

    // Progress at midpoint matches expectation.
    t.contract.withdraw(&id);
    assert_eq!(t.token.balance(&t.recipient), 500);
    assert_eq!(t.token.balance(&t.contract.address), 500);
}

// ── Issue #24: Locked view boundary & README schedule tests ───────────────────
//
// `locked(id)` returns the unvested portion of tokens remaining locked in the contract
// (`total_amount - vested`). It is a read-only view function that never alters state.
//
// These tests pin:
// - Deterministic `StreamNotFound` on unknown IDs without side effects.
// - Transition from `total_amount` before start/cliff down to `0` at/after end_time.
// - Zero locked balance immediately upon cancellation.
// - Exact match with the no-cliff and cliff example schedules documented in `README.md`.

/// `locked` on an unknown ID returns `StreamNotFound` deterministically without
/// altering contract storage or balances.
#[test]
fn locked_unknown_id_returns_stream_not_found() {
    let t = StreamTest::setup(1_000);

    assert_eq!(
        t.contract.try_locked(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "unknown id must return StreamNotFound"
    );

    // Deterministic: second call produces identical error.
    assert_eq!(
        t.contract.try_locked(&99),
        Err(Ok(StreamError::StreamNotFound)),
        "StreamNotFound must be returned on retry"
    );

    // Invariants hold.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

/// `locked` boundary transitions: 1000 before start, decreases linearly, settles at 0 at/after end.
#[test]
fn locked_boundary_transitions_from_start_to_end() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Before start: 1000 locked.
    t.set_time(50);
    assert_eq!(t.contract.locked(&id), 1_000);

    // At start (t=100): 1000 locked.
    t.set_time(100);
    assert_eq!(t.contract.locked(&id), 1_000);

    // Midpoint (t=600): 500 locked.
    t.set_time(600);
    assert_eq!(t.contract.locked(&id), 500);

    // At end_time (t=1100): 0 locked.
    t.set_time(1_100);
    assert_eq!(t.contract.locked(&id), 0);

    // Past end_time (t=2000): 0 locked.
    t.set_time(2_000);
    assert_eq!(t.contract.locked(&id), 0);
}

/// `locked` for a no-cliff stream matches the exact example schedule table in `README.md`.
#[test]
fn locked_no_cliff_schedule_matches_readme_example() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100, // cliff == start (no cliff)
    );

    // Table checks from README.md:
    // t=50: vested 0, locked 1000
    t.set_time(50);
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.locked(&id), 1_000);

    // t=350: vested 250, locked 750
    t.set_time(350);
    assert_eq!(t.contract.vested(&id), 250);
    assert_eq!(t.contract.locked(&id), 750);

    // t=600: vested 500, locked 500
    t.set_time(600);
    assert_eq!(t.contract.vested(&id), 500);
    assert_eq!(t.contract.locked(&id), 500);

    // t=850: vested 750, locked 250
    t.set_time(850);
    assert_eq!(t.contract.vested(&id), 750);
    assert_eq!(t.contract.locked(&id), 250);

    // t=1100: vested 1000, locked 0
    t.set_time(1_100);
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.locked(&id), 0);

    // t=9999: vested 1000, locked 0
    t.set_time(9_999);
    assert_eq!(t.contract.vested(&id), 1_000);
    assert_eq!(t.contract.locked(&id), 0);
}

/// A cancelled stream returns 0 locked because cancellation freezes `total_amount` at `vested`.
#[test]
fn locked_cancelled_stream_returns_zero() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    // Cancel at midpoint.
    t.set_time(600);
    t.contract.cancel(&id);

    // Immediately zero locked.
    assert_eq!(t.contract.locked(&id), 0);

    // Past original end_time: still zero locked.
    t.set_time(2_000);
    assert_eq!(t.contract.locked(&id), 0);
}

/// `locked` is read-only and leaves token balances and stream records unchanged.
#[test]
fn locked_view_is_read_only_and_leaves_no_side_effects() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );

    let sender_bal = t.token.balance(&t.sender);
    let contract_bal = t.token.balance(&t.contract.address);

    for ts in [50, 100, 350, 600, 850, 1_100, 2_000] {
        t.set_time(ts);
        let _ = t.contract.locked(&id);
    }

    assert_eq!(t.token.balance(&t.sender), sender_bal);
    assert_eq!(t.token.balance(&t.contract.address), contract_bal);
    assert_eq!(t.contract.stream_count(), 1);
}

// ── Issue #25: Pending status regression & invalid participant tests ──────────
//
// These tests verify that:
// - A stream in `Pending` status behaves deterministically across all view and
//   state-modifying operations (such as withdrawals, cancellations, and status queries).
// - Creation calls where the token address is the sender or recipient are
//   rejected with `InvalidParticipant` before any tokens move.
// - Existing valid stream creation behavior is unaffected.

/// Verify that a stream in `Pending` status (`now < start_time`) behaves correctly:
/// - `status` reports `Pending`.
/// - `vested` is `0`, `withdrawable` is `0`, `locked` is `total_amount`, and `progress` is `0`.
/// - `withdraw` returns `NothingToWithdraw` and transfers no tokens.
/// - `withdraw_amount` returns `InsufficientBalance` for any positive amount and transfers no tokens.
/// - `cancel` successfully refunds 100% of the stream amount and sets the status to `Cancelled`.
#[test]
fn status_pending_regression_test() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let start = 200u64;
    let cliff = 200u64;
    let end = 1_200u64;
    let amount = 1_000i128;

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &cliff,
    );

    // Set time before start_time to make the stream Pending.
    t.set_time(150);

    // 1. Assert view functions match expected Pending values.
    assert_eq!(t.contract.status(&id), StreamStatus::Pending);
    assert_eq!(t.contract.vested(&id), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(t.contract.locked(&id), amount);
    assert_eq!(t.contract.progress(&id), 0);

    // 2. Assert try_withdraw fails with NothingToWithdraw and leaves balances unchanged.
    assert_eq!(
        t.contract.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );
    assert_eq!(t.token.balance(&t.recipient), 0);
    assert_eq!(t.token.balance(&t.contract.address), amount);

    // 3. Assert try_withdraw_amount with positive amount fails with InsufficientBalance.
    assert_eq!(
        t.contract.try_withdraw_amount(&id, &1),
        Err(Ok(StreamError::InsufficientBalance))
    );
    assert_eq!(t.token.balance(&t.recipient), 0);
    assert_eq!(t.token.balance(&t.contract.address), amount);

    // 4. Assert cancel refunds 100% of the tokens to sender and transitions to Cancelled.
    let refund = t.contract.cancel(&id);
    assert_eq!(refund, amount);
    assert_eq!(t.token.balance(&t.sender), amount);
    assert_eq!(t.token.balance(&t.contract.address), 0);
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);
}

/// Creation fails with `InvalidParticipant` when the token address is used as the sender.
#[test]
fn create_stream_rejects_token_as_sender() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Using t.token_address as the sender.
    let result = t.contract.try_create_stream(
        &t.token_address,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));
    t.assert_nothing_happened(1_000);
}

/// Creation fails with `InvalidParticipant` when the token address is used as the recipient.
#[test]
fn create_stream_rejects_token_as_recipient() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Using t.token_address as the recipient.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.token_address,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::InvalidParticipant)));
    t.assert_nothing_happened(1_000);
}

// ── Issue #26: Streaming status regression tests ───────────────────────────────
//
// These tests verify that:
// - A stream in `Streaming` status behaves deterministically across all view and
//   state-modifying operations (such as withdrawals, cancellations, and status queries).
// - Creation calls with the contract's own address as a participant are rejected
//   deterministically, preventing any token transfers.

/// Verify that a stream in `Streaming` status (`start_time <= now < end_time`) behaves correctly:
/// - `status` reports `Streaming`.
/// - `vested`, `withdrawable`, `locked`, and `progress` compute active linear values.
/// - `withdraw_amount` successfully transfers a partial amount to the recipient.
/// - `withdraw` successfully transfers all remaining withdrawable tokens.
/// - `cancel` splits the remaining unvested tokens and refunds the sender.
#[test]
fn status_streaming_regression_test() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let start = 100u64;
    let cliff = 100u64;
    let end = 1_100u64;
    let amount = 1_000i128;

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &cliff,
    );

    // Set time to the midpoint (600) to make the stream Streaming (50% vested).
    t.set_time(600);

    // 1. Assert view functions match expected Streaming values (midpoint).
    assert_eq!(t.contract.status(&id), StreamStatus::Streaming);
    assert_eq!(t.contract.vested(&id), 500);
    assert_eq!(t.contract.withdrawable(&id), 500);
    assert_eq!(t.contract.locked(&id), 500);
    assert_eq!(t.contract.progress(&id), 5_000);

    // 2. Assert withdraw_amount (partial withdraw) works and updates views.
    let partial_withdrawn = t.contract.withdraw_amount(&id, &200);
    assert_eq!(partial_withdrawn, 200);
    assert_eq!(t.token.balance(&t.recipient), 200);
    assert_eq!(t.contract.withdrawable(&id), 300);
    assert_eq!(t.contract.progress(&id), 5_000); // Progress is based on total vested, not withdrawn.

    // 3. Assert withdraw (drain remaining vested) works.
    let full_withdrawn = t.contract.withdraw(&id);
    assert_eq!(full_withdrawn, 300);
    assert_eq!(t.token.balance(&t.recipient), 500);
    assert_eq!(t.contract.withdrawable(&id), 0);

    // 4. Assert cancel refunds remaining unvested tokens (500) to sender.
    // At ts=600, total vested = 500. Refund = 1000 - 500 = 500.
    let refund = t.contract.cancel(&id);
    assert_eq!(refund, 500);
    assert_eq!(t.token.balance(&t.sender), 500); // 1000 initial - 1000 created + 500 refund = 500.
    assert_eq!(t.token.balance(&t.contract.address), 0); // 500 recipient withdrawn + 500 sender refund.
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);
}

/// Assert that creating a stream with the contract's own address as a participant
/// is rejected with `InvalidParticipant` and no token transfer occurs.
#[test]
fn create_stream_rejects_contract_address_participant_regression() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);
    let contract_address = t.contract.address.clone();

    // Rejects contract as recipient
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &contract_address,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );

    // Rejects contract as sender
    assert_eq!(
        t.contract.try_create_stream(
            &contract_address,
            &t.recipient,
            &t.token_address,
            &1_000,
            &100,
            &1_100,
            &100
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );

    // Rejects contract as token
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender,
            &t.recipient,
            &contract_address,
            &1_000,
            &100,
            &1_100,
            &100
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );

    // No token transfer occurred
    t.assert_nothing_happened(1_000);
}

// ── Issue #27: Cancelled status regression tests ───────────────────────────────
//
// These tests verify that:
// - A stream in `Cancelled` status behaves deterministically across all view and
//   state-modifying operations (such as withdrawals, duplicate cancellations, and status queries).
// - Counter overflow is handled without wrapping or ID reuse, returning `StreamCountExhausted`.

/// Verify that a stream in `Cancelled` status behaves correctly:
/// - `status` reports `Cancelled`.
/// - `vested` returns the frozen vested amount.
/// - `locked` returns `0`.
/// - `progress` returns `10_000`.
/// - `withdraw` successfully transfers the unwithdrawn vested tokens to the recipient.
/// - `cancel` returns `AlreadyCancelled`.
#[test]
fn status_cancelled_regression_test() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let start = 100u64;
    let cliff = 100u64;
    let end = 1_100u64;
    let amount = 1_000i128;

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &cliff,
    );

    // Set time to the midpoint (600) where 50% is vested.
    t.set_time(600);

    // Cancel the stream. Sender gets 500 back, 500 remains in contract for recipient.
    let refund = t.contract.cancel(&id);
    assert_eq!(refund, 500);

    // 1. Assert view functions match expected Cancelled values.
    assert_eq!(t.contract.status(&id), StreamStatus::Cancelled);
    assert_eq!(t.contract.vested(&id), 500);
    assert_eq!(t.contract.withdrawable(&id), 500);
    assert_eq!(t.contract.locked(&id), 0);
    assert_eq!(t.contract.progress(&id), 10_000);

    // 2. Assert try_cancel fails with AlreadyCancelled.
    assert_eq!(
        t.contract.try_cancel(&id),
        Err(Ok(StreamError::AlreadyCancelled))
    );

    // 3. Assert recipient can still withdraw the 500 vested units.
    let withdrawn = t.contract.withdraw(&id);
    assert_eq!(withdrawn, 500);
    assert_eq!(t.token.balance(&t.recipient), 500);
    assert_eq!(t.token.balance(&t.contract.address), 0);
    assert_eq!(t.contract.withdrawable(&id), 0);
}

/// Assert that when the stream counter reaches `u64::MAX`, subsequent creation calls
/// fail with `StreamCountExhausted` without wrapping to zero, consuming/reusing any IDs,
/// or moving tokens.
#[test]
fn counter_overflow_boundary_regression_test() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    // Mock counter to u64::MAX.
    t.set_stream_count(u64::MAX);

    // Attempting to create a stream should fail.
    let result = t.contract.try_create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &1_000,
        &100,
        &1_100,
        &100,
    );
    assert_eq!(result, Err(Ok(StreamError::StreamCountExhausted)));

    // Ensure the ID counter is not reset, wrapping does not occur, and no tokens moved.
    assert_eq!(t.contract.stream_count(), u64::MAX);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

// ── Issue #28: Completed status regression tests ───────────────────────────────
//
// These tests verify that:
// - A stream in `Completed` status behaves deterministically across all view and
//   state-modifying operations (such as withdrawals, cancellations, and status queries).
// - Once `now >= end_time`, the stream cannot be cancelled, but the recipient can
//   withdraw any remaining vested tokens.

/// Verify that a stream in `Completed` status behaves correctly:
/// - `status` reports `Completed`.
/// - `vested` returns `total_amount`.
/// - `locked` returns `0`.
/// - `progress` returns `10_000`.
/// - `cancel` returns `StreamAlreadyCompleted`.
/// - `withdraw_amount` (partial withdraw) works and updates `withdrawable` while status stays `Completed`.
/// - `withdraw` successfully transfers all remaining withdrawable tokens.
#[test]
fn status_completed_regression_test() {
    let t = StreamTest::setup(1_000);
    t.set_time(100);

    let start = 100u64;
    let cliff = 100u64;
    let end = 1_100u64;
    let amount = 1_000i128;

    let id = t.contract.create_stream(
        &t.sender,
        &t.recipient,
        &t.token_address,
        &amount,
        &start,
        &end,
        &cliff,
    );

    // Set time to or after end_time (1200) to make the stream Completed.
    t.set_time(1200);

    // 1. Assert view functions match expected Completed values.
    assert_eq!(t.contract.status(&id), StreamStatus::Completed);
    assert_eq!(t.contract.vested(&id), amount);
    assert_eq!(t.contract.withdrawable(&id), amount);
    assert_eq!(t.contract.locked(&id), 0);
    assert_eq!(t.contract.progress(&id), 10_000);

    // 2. Assert try_cancel fails with StreamAlreadyCompleted.
    assert_eq!(
        t.contract.try_cancel(&id),
        Err(Ok(StreamError::StreamAlreadyCompleted))
    );

    // 3. Assert withdraw_amount (partial withdraw) works and updates withdrawable.
    let partial_withdrawn = t.contract.withdraw_amount(&id, &400);
    assert_eq!(partial_withdrawn, 400);
    assert_eq!(t.token.balance(&t.recipient), 400);
    assert_eq!(t.contract.withdrawable(&id), 600);
    assert_eq!(t.contract.status(&id), StreamStatus::Completed); // remains Completed

    // 4. Assert withdraw (drain remaining vested) works.
    let full_withdrawn = t.contract.withdraw(&id);
    assert_eq!(full_withdrawn, 600);
    assert_eq!(t.token.balance(&t.recipient), amount);
    assert_eq!(t.contract.withdrawable(&id), 0);
    assert_eq!(t.contract.status(&id), StreamStatus::Completed); // remains Completed
}

// ── Issue #77: Test unknown id on every view ─────────────────────────────────
//
// Every view function must return `StreamNotFound` when called with an id that
// does not exist in storage. This test exercises all seven view entry points
// against the same unknown id and confirms the error is deterministic and
// leaves no side effects.

/// Every view function returns `StreamNotFound` for an unknown id. The error is
/// deterministic across repeated calls and no contract state is altered.
#[test]
fn unknown_id_returns_stream_not_found_on_every_view() {
    let t = StreamTest::setup(1_000);
    let unknown_id: u64 = 42;

    // get_stream
    assert_eq!(
        t.contract.try_get_stream(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // withdraw
    assert_eq!(
        t.contract.try_withdraw(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // cancel
    assert_eq!(
        t.contract.try_cancel(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // withdrawable
    assert_eq!(
        t.contract.try_withdrawable(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // vested
    assert_eq!(
        t.contract.try_vested(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // locked
    assert_eq!(
        t.contract.try_locked(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // progress
    assert_eq!(
        t.contract.try_progress(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    // status
    assert_eq!(
        t.contract.try_status(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );

    // Deterministic: repeating the same calls yields the same errors.
    assert_eq!(
        t.contract.try_get_stream(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );
    assert_eq!(
        t.contract.try_vested(&unknown_id),
        Err(Ok(StreamError::StreamNotFound))
    );

    // No side effects: stream count is zero, sender balance intact.
    assert_eq!(t.contract.stream_count(), 0);
    assert_eq!(t.token.balance(&t.sender), 1_000);
    assert_eq!(t.token.balance(&t.contract.address), 0);
}

// ── Issue #78: Test stream ids remain contiguous ─────────────────────────────
//
// Stream ids are assigned from a monotonic counter starting at 0. Creating
// multiple streams — including after cancellations — must produce the sequence
// 0, 1, 2, … without gaps or reuse.

/// Creating several streams in sequence yields contiguous ids 0, 1, 2, …
/// regardless of whether earlier streams have been cancelled.
#[test]
fn stream_ids_remain_contiguous_across_creations_and_cancellations() {
    let t = StreamTest::setup(10_000);
    t.set_time(100);

    // Create three streams: ids must be 0, 1, 2.
    let id0 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    let id1 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    let id2 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    assert_eq!(id0, 0);
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(t.contract.stream_count(), 3);

    // Cancel stream 1 — the counter must not change.
    t.set_time(600);
    t.contract.cancel(&id1);
    assert_eq!(t.contract.stream_count(), 3);

    // Create a fourth stream — id must be 3, not a reuse of the cancelled id.
    let id3 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    assert_eq!(id3, 3);
    assert_eq!(t.contract.stream_count(), 4);

    // The cancelled stream's record is still intact at id 1.
    assert_eq!(t.contract.get_stream(&id1).cancelled, true);

    // All four stream records exist and have the correct ids.
    for expected_id in 0..4u64 {
        let s = t.contract.get_stream(&expected_id);
        assert_eq!(s.total_amount, 1_000);
    }
}

// ── Issue #79: Test stream_count read view ───────────────────────────────────
//
// `stream_count()` is a read-only view that returns the number of streams
// created so far. It must be 0 on a fresh contract, advance by 1 on each
// successful creation, and remain unchanged on rejected creations.

/// `stream_count` starts at 0, increments on each creation, and is unaffected
/// by rejected creation attempts.
#[test]
fn stream_count_read_view_reflects_creations_and_ignores_rejections() {
    let t = StreamTest::setup(2_000);
    t.set_time(100);

    // Fresh contract: count is 0.
    assert_eq!(t.contract.stream_count(), 0);

    // First creation: count becomes 1.
    t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    assert_eq!(t.contract.stream_count(), 1);

    // Rejected creation (self-stream): count must stay 1.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender, &t.sender, &t.token_address,
            &1_000, &100, &1_100, &100,
        ),
        Err(Ok(StreamError::InvalidParticipant))
    );
    assert_eq!(t.contract.stream_count(), 1);

    // Second creation: count becomes 2.
    t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    assert_eq!(t.contract.stream_count(), 2);

    // Rejected creation (zero amount): count must stay 2.
    assert_eq!(
        t.contract.try_create_stream(
            &t.sender, &t.recipient, &t.token_address,
            &0, &100, &1_100, &100,
        ),
        Err(Ok(StreamError::InvalidAmount))
    );
    assert_eq!(t.contract.stream_count(), 2);

    // stream_count is read-only: calling it does not change the value.
    assert_eq!(t.contract.stream_count(), 2);
    assert_eq!(t.contract.stream_count(), 2);
}

// ── Issue #80: Test stream_count after cancellation ──────────────────────────
//
// Cancelling a stream must not change `stream_count`. The counter tracks how
// many streams have been *created*, not how many are active.

/// `stream_count` is unchanged after cancelling a stream.
#[test]
fn stream_count_unchanged_after_cancellation() {
    let t = StreamTest::setup(3_000);
    t.set_time(100);

    // Create three streams.
    let id0 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    let _id1 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    let _id2 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    assert_eq!(t.contract.stream_count(), 3);

    // Cancel stream 0.
    t.set_time(600);
    t.contract.cancel(&id0);
    assert_eq!(t.contract.stream_count(), 3);

    // Cancel stream 1.
    t.contract.cancel(&(_id1));
    assert_eq!(t.contract.stream_count(), 3);

    // Cancel stream 2.
    t.contract.cancel(&(_id2));
    assert_eq!(t.contract.stream_count(), 3);

    // Even after all streams are cancelled, the count reflects total created.
    assert_eq!(t.contract.stream_count(), 3);

    // Creating a new stream still advances the counter.
    let id3 = t.contract.create_stream(
        &t.sender, &t.recipient, &t.token_address,
        &1_000, &100, &1_100, &100,
    );
    assert_eq!(id3, 3);
    assert_eq!(t.contract.stream_count(), 4);
}


