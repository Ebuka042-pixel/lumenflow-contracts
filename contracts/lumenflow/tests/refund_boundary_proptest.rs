/// Property-based tests for refund amount boundary conditions — issue #1360.
///
/// Uses [`proptest`] to generate random refund amounts, cumulative totals,
/// and timestamps, verifying that the contract correctly accepts every valid
/// combination and rejects every invalid one.
///
/// Acceptance criteria:
///   AC1 — any amount < min_refund_amount is rejected with RefundBelowMinimum
///   AC2 — any amount > remaining_refundable is rejected with RefundExceedsOriginal
///   AC3 — any initiation timestamp > paid_at + refund_window is rejected with RefundWindowExpired
///   AC4 — valid inputs within all bounds are accepted

extern crate alloc;

use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Address, Env, String,
};

use lumenflow::{
    error::PaymentError, MerchantCategory, PaymentProcessingContract,
    PaymentProcessingContractClient,
};

// ── Contract-level constants (must stay in sync with storage.rs) ──────────────

/// Default minimum refund amount (storage::MIN_REFUND_AMOUNT).
const DEFAULT_MIN_REFUND: i128 = 100;
/// Default refund window in seconds (storage::DEFAULT_REFUND_WINDOW_SECS = 30 days).
const DEFAULT_REFUND_WINDOW: u64 = 30 * 24 * 3600;
/// A large payment amount used as the base for all property tests.
const PAYMENT_AMOUNT: i128 = 1_000_000;
/// Token supply minted to the payer for each test environment.
const PAYER_BALANCE: i128 = 10_000_000;

// ── Shared setup ──────────────────────────────────────────────────────────────

/// Bootstraps a fresh Soroban test environment with:
/// - A deployed LumenFlow contract
/// - An admin, a registered merchant, and a funded payer
/// - One completed payment of `PAYMENT_AMOUNT` to use as the base for refunds
///
/// Returns `(env, client, admin, merchant, payer, token, order_id)`.
fn setup_with_payment() -> (
    Env,
    PaymentProcessingContractClient<'static>,
    Address, // admin
    Address, // merchant
    Address, // payer
    Address, // token
    soroban_sdk::String, // order_id
) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(PaymentProcessingContract, ());
    let client = PaymentProcessingContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    let payer = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    // Initialise contract state
    client.set_admin(&admin);
    client.add_allowed_token(&admin, &token);

    // Register merchant
    client.register_merchant(
        &merchant,
        &String::from_str(&env, "PropTest Store"),
        &String::from_str(&env, "Property-based test merchant"),
        &String::from_str(&env, "prop@example.com"),
        &MerchantCategory::Retail,
    );

    // Fund payer and approve token
    StellarAssetClient::new(&env, &token).mint(&payer, &PAYER_BALANCE);

    // Process a payment so there is something to refund
    let order_id = String::from_str(&env, "ORDER_PROP_001");
    client.create_payment_request(
        &merchant,
        &order_id,
        &token,
        &PAYMENT_AMOUNT,
        &String::from_str(&env, "PropTest invoice"),
        &(DEFAULT_REFUND_WINDOW * 2), // generous TTL
    );
    client.pay_payment_request(&payer, &order_id);

    (env, client, admin, merchant, payer, token, order_id)
}

// ── AC1: amount < min_refund_amount → RefundBelowMinimum ─────────────────────

proptest! {
    /// Any refund amount strictly below the configured minimum must be rejected
    /// with `RefundBelowMinimum`, regardless of how small it is.
    #[test]
    fn prop_amount_below_minimum_is_rejected(
        // Generate amounts in [1, DEFAULT_MIN_REFUND - 1]
        amount in 1_i128..DEFAULT_MIN_REFUND,
        // A unique suffix so every proptest run uses a fresh refund ID
        suffix in 0_u32..100_000,
    ) {
        let (env, client, _admin, merchant, _payer, _token, order_id) =
            setup_with_payment();

        let refund_id = String::from_str(
            &env,
            alloc::format!("REFUND_BELOW_{suffix}").as_str(),
        );

        let result = client.try_initiate_refund(
            &merchant,
            &refund_id,
            &order_id,
            &amount,
            &String::from_str(&env, "below minimum"),
        );

        prop_assert_eq!(
            result,
            Err(Ok(PaymentError::RefundBelowMinimum)),
            "amount={amount} should be rejected as below minimum ({DEFAULT_MIN_REFUND})"
        );
    }
}

// ── AC2: amount > remaining_refundable → RefundExceedsOriginal ────────────────

proptest! {
    /// Any refund amount that exceeds the remaining refundable balance must be
    /// rejected with `RefundExceedsOriginal`.
    ///
    /// The test sets `already_refunded` stroops as the existing refunded amount
    /// (by approving and executing a first partial refund), then attempts a
    /// second refund for more than what remains.
    #[test]
    fn prop_amount_exceeds_remaining_is_rejected(
        // already_refunded: a valid first refund in [DEFAULT_MIN_REFUND, PAYMENT_AMOUNT / 2]
        already_refunded in DEFAULT_MIN_REFUND..=(PAYMENT_AMOUNT / 2),
        // excess: extra amount on top of the remaining balance, in [1, PAYMENT_AMOUNT]
        excess in 1_i128..=PAYMENT_AMOUNT,
        suffix in 0_u32..100_000,
    ) {
        let (env, client, _admin, merchant, _payer, _token, order_id) =
            setup_with_payment();

        // Step 1: issue a valid first partial refund to consume `already_refunded` stroops
        let refund_id_1 = String::from_str(
            &env,
            alloc::format!("REFUND_FIRST_{suffix}").as_str(),
        );
        client.initiate_refund(
            &merchant,
            &refund_id_1,
            &order_id,
            &already_refunded,
            &String::from_str(&env, "first partial"),
        );
        client.approve_refund(&merchant, &refund_id_1);
        client.execute_refund(&refund_id_1);

        // Step 2: attempt to refund more than what remains
        let remaining = PAYMENT_AMOUNT - already_refunded;
        let over_amount = remaining + excess; // always > remaining

        let refund_id_2 = String::from_str(
            &env,
            alloc::format!("REFUND_OVER_{suffix}").as_str(),
        );
        let result = client.try_initiate_refund(
            &merchant,
            &refund_id_2,
            &order_id,
            &over_amount,
            &String::from_str(&env, "over remaining"),
        );

        prop_assert_eq!(
            result,
            Err(Ok(PaymentError::RefundExceedsOriginal)),
            "over_amount={over_amount} (remaining={remaining}) should be rejected"
        );
    }
}

// ── AC3: timestamp > paid_at + refund_window → RefundWindowExpired ────────────

proptest! {
    /// Any refund initiated after the refund window has closed must be rejected
    /// with `RefundWindowExpired`.
    #[test]
    fn prop_expired_timestamp_is_rejected(
        // How many seconds past the window boundary (at least 1 second)
        seconds_past_window in 1_u64..=(365 * 24 * 3600_u64),
        suffix in 0_u32..100_000,
    ) {
        let (env, client, _admin, merchant, _payer, _token, order_id) =
            setup_with_payment();

        // Advance ledger time past the refund window
        let paid_at = env.ledger().timestamp();
        env.ledger().with_mut(|l| {
            l.timestamp = paid_at + DEFAULT_REFUND_WINDOW + seconds_past_window;
        });

        let refund_id = String::from_str(
            &env,
            alloc::format!("REFUND_EXPIRED_{suffix}").as_str(),
        );

        let result = client.try_initiate_refund(
            &merchant,
            &refund_id,
            &order_id,
            &DEFAULT_MIN_REFUND,
            &String::from_str(&env, "too late"),
        );

        prop_assert_eq!(
            result,
            Err(Ok(PaymentError::RefundWindowExpired)),
            "timestamp {seconds_past_window}s past window should be rejected"
        );
    }
}

// ── AC4: valid inputs within all bounds are accepted ─────────────────────────

proptest! {
    /// Any refund where amount ∈ [min_refund, payment_amount] and the ledger
    /// timestamp is within the window must be accepted.
    #[test]
    fn prop_valid_refund_within_bounds_is_accepted(
        // amount: valid single refund in [DEFAULT_MIN_REFUND, PAYMENT_AMOUNT]
        amount in DEFAULT_MIN_REFUND..=PAYMENT_AMOUNT,
        // seconds_before_deadline: how far inside the window (at least 1 second before expiry)
        seconds_before_deadline in 1_u64..DEFAULT_REFUND_WINDOW,
        suffix in 0_u32..100_000,
    ) {
        let (env, client, _admin, merchant, _payer, _token, order_id) =
            setup_with_payment();

        // Advance ledger to just before the window closes
        let paid_at = env.ledger().timestamp();
        env.ledger().with_mut(|l| {
            l.timestamp = paid_at + DEFAULT_REFUND_WINDOW - seconds_before_deadline;
        });

        let refund_id = String::from_str(
            &env,
            alloc::format!("REFUND_VALID_{suffix}").as_str(),
        );

        let result = client.try_initiate_refund(
            &merchant,
            &refund_id,
            &order_id,
            &amount,
            &String::from_str(&env, "valid refund"),
        );

        prop_assert!(
            result.is_ok(),
            "valid refund (amount={amount}, {seconds_before_deadline}s before deadline) \
             should be accepted, got {result:?}"
        );
    }
}

// ── Additional edge-case: zero amount always rejected ─────────────────────────

/// Zero is never a valid refund amount — `require_positive` rejects it before
/// the minimum check.
#[test]
fn test_zero_amount_always_rejected() {
    let (env, client, _admin, merchant, _payer, _token, order_id) = setup_with_payment();

    let result = client.try_initiate_refund(
        &merchant,
        &String::from_str(&env, "REFUND_ZERO"),
        &order_id,
        &0_i128,
        &String::from_str(&env, "zero"),
    );

    // require_positive fires before require_min_refund_amount
    assert!(
        matches!(result, Err(Ok(PaymentError::InvalidAmount)) | Err(Ok(PaymentError::RefundBelowMinimum))),
        "zero amount must be rejected, got {result:?}"
    );
}

/// Exactly the minimum refund amount must be accepted (boundary inclusivity).
#[test]
fn test_exact_minimum_amount_accepted() {
    let (env, client, _admin, merchant, _payer, _token, order_id) = setup_with_payment();

    let result = client.try_initiate_refund(
        &merchant,
        &String::from_str(&env, "REFUND_MIN_EXACT"),
        &order_id,
        &DEFAULT_MIN_REFUND,
        &String::from_str(&env, "exact minimum"),
    );

    assert!(
        result.is_ok(),
        "refund equal to minimum should be accepted, got {result:?}"
    );
}

/// Exactly at the refund window boundary (paid_at + refund_window) must still
/// be accepted (the window check uses `>`, not `>=`).
#[test]
fn test_refund_at_window_boundary_is_accepted() {
    let (env, client, _admin, merchant, _payer, _token, order_id) = setup_with_payment();

    let paid_at = env.ledger().timestamp();
    env.ledger().with_mut(|l| {
        l.timestamp = paid_at + DEFAULT_REFUND_WINDOW;
    });

    let result = client.try_initiate_refund(
        &merchant,
        &String::from_str(&env, "REFUND_BOUNDARY"),
        &order_id,
        &DEFAULT_MIN_REFUND,
        &String::from_str(&env, "boundary"),
    );

    assert!(
        result.is_ok(),
        "refund exactly at window boundary should be accepted, got {result:?}"
    );
}

/// One second past the boundary must be rejected.
#[test]
fn test_refund_one_second_past_window_is_rejected() {
    let (env, client, _admin, merchant, _payer, _token, order_id) = setup_with_payment();

    let paid_at = env.ledger().timestamp();
    env.ledger().with_mut(|l| {
        l.timestamp = paid_at + DEFAULT_REFUND_WINDOW + 1;
    });

    let result = client.try_initiate_refund(
        &merchant,
        &String::from_str(&env, "REFUND_PAST"),
        &order_id,
        &DEFAULT_MIN_REFUND,
        &String::from_str(&env, "just expired"),
    );

    assert_eq!(result, Err(Ok(PaymentError::RefundWindowExpired)));
}
