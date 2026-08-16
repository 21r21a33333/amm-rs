//! Shared revm-fork test harness.
//!
//! Provides [`Fork`], an in-process EVM fork backend backed by
//! `foundry-fork-db` + `revm`.  It exposes ERC-20 slot-stuffing,
//! snapshot/revert, and transaction-submission helpers used by all integration
//! tests.
//!
//! Architecture:
//! - [`foundry_fork_db::SharedBackend`] fetches missing account/storage data
//!   from the RPC and caches it to a JSON file at
//!   `<workspace-root>/target/revm-fork-cache/<block>.json`.
//! - [`revm::database::CacheDB`] wraps the backend and holds all dirty state
//!   from committed transactions.
//! - `revm` executes transactions synchronously in-process via
//!   `Context::mainnet().with_db(…).build_mainnet().transact_commit(…)`.
//! - A generic alloy provider `P` is held for pool-state reads that require a
//!   live RPC connection (e.g. [`StateSource::refresh`]).
//!
//! Requires `flavor = "multi_thread"` in the calling `#[tokio::test]` because
//! [`foundry_fork_db::SharedBackend`] calls `tokio::task::block_in_place`
//! internally to park its RPC-polling thread.

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, Bytes, U256, keccak256};
use alloy::providers::Provider;
use alloy::sol;
use amm_core::primitives::asset::ChainId;
use amm_rpc::execution::{PreparedSwap, UnsignedTx};
use foundry_fork_db::{BlockchainDb, SharedBackend, cache::BlockchainDbMeta};
use revm::Context;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::{CacheDB, WrapDatabaseRef};
use revm::handler::{ExecuteCommitEvm, ExecuteEvm, MainBuilder, MainContext};
use revm::primitives::{KECCAK_EMPTY, TxKind, hardfork::SpecId};
use revm::state::AccountInfo;
use std::path::PathBuf;
use std::sync::Arc;

// ── local ERC-20 interface (balanceOf / allowance / approve) ─────────────────

sol! {
    /// Minimal ERC-20 interface used for balance reads and approvals in tests.
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

// ── Permit2 interface (signature-free allowance path) ─────────────────────────

sol! {
    /// Permit2's on-chain allowance setter.  Called after the ERC-20 has
    /// approved Permit2, this grants `spender` (e.g. the Universal Router) a
    /// time-bounded allowance inside Permit2's own accounting.
    interface IPermit2 {
        function approve(address token, address spender, uint160 amount, uint48 expiration) external;
    }
}

// ── Fork ─────────────────────────────────────────────────────────────────────

/// The concrete `CacheDB` type used by [`Fork`].
type ForkDb = CacheDB<WrapDatabaseRef<SharedBackend>>;

/// In-process EVM fork backed by `foundry-fork-db` + `revm`.
///
/// Generic over the alloy provider `P` used by pool-state helpers that require
/// a live RPC connection (e.g. [`StateSource::refresh`]).  Only [`fork_at`] is
/// `async`; all mutation methods are synchronous.
pub struct Fork<P> {
    /// Mutable EVM state layer.  Held as `Option` so it can be temporarily
    /// moved into a `Context` for execution and then reclaimed.
    db: Option<ForkDb>,
    /// Pinned block number.
    block_number: u64,
    /// Pre-built `BlockEnv` for the pinned block (basefee == 0).
    block_env: BlockEnv,
    /// Chain ID used by approval and execution transactions.
    chain: ChainId,
    /// Alloy provider for pool-state reads.
    provider: P,
}

/// Build a disk-cached in-process fork pinned to `block`.
///
/// `block_env.basefee` is forced to `0` so that legacy zero-gas-price
/// transactions are accepted without the base-fee check.  The cache is
/// stored at `<workspace-root>/target/revm-fork-cache/<block>.json` and
/// shared across test runs to minimise RPC round-trips.
pub async fn fork_at<P>(rpc_url: &str, block: u64, chain: ChainId, provider: P) -> Fork<P>
where
    P: Provider + Clone + Send + Sync + 'static,
{
    // ── alloy v1.x provider for foundry-fork-db ───────────────────────────────
    let fdb_provider = alloy_provider::ProviderBuilder::new()
        .network::<alloy_provider::network::AnyNetwork>()
        .connect_client(
            alloy_rpc_client::ClientBuilder::default()
                .http(rpc_url.parse().expect("invalid RPC url")),
        );

    // ── disk cache path ───────────────────────────────────────────────────────
    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("revm-fork-cache");
    let cache_path = cache_dir.join(format!("{block}.json"));

    // ── Fetch the pinned block's real timestamp ───────────────────────────────
    // Timestamp 0 is unacceptable: Solidly/Aerodrome oracle arithmetic does
    // `block.timestamp - lastTimestamp` in unchecked integer subtraction, so
    // a zero timestamp against any real `lastTimestamp` (~1.7e9) would
    // underflow and panic (EVM error 0x11).  Fetch the real value from the
    // RPC; fall back to a sane non-zero sentinel only if the fetch fails.
    let block_timestamp: u64 = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await
        .ok()
        .flatten()
        .map(|b| b.header.timestamp)
        .unwrap_or(2_000_000_000u64);

    // ── BlockEnv pinned to `block` with zero basefee ──────────────────────────
    let block_env = BlockEnv {
        number: U256::from(block),
        basefee: 0u64,
        gas_limit: 30_000_000u64,
        timestamp: U256::from(block_timestamp),
        ..Default::default()
    };

    // ── foundry-fork-db setup ─────────────────────────────────────────────────
    let meta = BlockchainDbMeta::new(block_env.clone(), rpc_url.to_owned());
    let blockchain_db = BlockchainDb::new(meta, Some(cache_path));
    let block_id = alloy_rpc_types::BlockId::number(block);
    let backend =
        SharedBackend::spawn_backend(Arc::new(fdb_provider), blockchain_db, Some(block_id)).await;

    let db = CacheDB::new(WrapDatabaseRef(backend));

    Fork {
        db: Some(db),
        block_number: block,
        block_env,
        chain,
        provider,
    }
}

impl<P: Provider + Clone> Fork<P> {
    // ── chain reads ──────────────────────────────────────────────────────────

    /// Pinned block number of this fork.
    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Return a reference to the underlying alloy provider.
    ///
    /// Tests that need to read on-chain state (e.g. refreshing a pool via a
    /// [`StateSource`]) can borrow or clone the provider from here.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    // ── slot stuffing ────────────────────────────────────────────────────────

    /// Write `amount` directly into the ERC-20 `balanceOf` storage slot for
    /// `holder`, then ensure `holder` has a high ETH balance for gas.
    ///
    /// `balance_slot` is the Solidity `mapping(address => uint256)` base slot
    /// (must be < 256 for the single-byte fast path used here).
    pub fn fund_erc20(&mut self, holder: Address, token: Address, balance_slot: u64, amount: U256) {
        debug_assert!(balance_slot < 256, "wide slots need full-word encoding");
        // Solidity mapping slot: keccak256( key ++ base_slot )
        // Layout (64 bytes): [0..12) = zero pad, [12..32) = holder, [32..64) = slot big-endian
        let mut pre = [0u8; 64];
        pre[12..32].copy_from_slice(holder.as_slice());
        // base slot fits in one byte; place it in the last byte of the 32-byte word
        pre[63] = balance_slot as u8;
        let slot = U256::from_be_bytes(*keccak256(pre));

        let db = self.db.as_mut().expect("db must be present");
        db.insert_account_storage(token, slot, amount)
            .expect("insert_account_storage for ERC-20 balance");

        // 100 ETH = 10^20 wei so the impersonated sender can cover any gas
        let eth_100 = U256::from(10u64).pow(U256::from(20u64));
        Self::set_balance_in_db(db, holder, eth_100);
    }

    /// Set the native-token (ETH) balance of `who` to `amount`.
    pub fn fund_native(&mut self, who: Address, amount: U256) {
        let db = self.db.as_mut().expect("db must be present");
        Self::set_balance_in_db(db, who, amount);
    }

    // ── approvals ────────────────────────────────────────────────────────────

    /// Approve `spender` to spend `amount` of `token` from `from`.
    ///
    /// Handles the USDT/KNC zero-first hazard: if the current allowance is
    /// non-zero and `amount` is also non-zero, `approve(spender, 0)` is sent
    /// first.
    pub fn approve(&mut self, from: Address, token: Address, spender: Address, amount: U256) {
        use alloy::sol_types::SolCall as _;
        // Read current allowance via a static call; reset to zero first if needed.
        let current_allowance = self.call_erc20_allowance(from, token, spender);

        if !current_allowance.is_zero() && !amount.is_zero() {
            let reset_data: Bytes = IERC20::approveCall::abi_encode(&IERC20::approveCall {
                spender,
                amount: U256::ZERO,
            })
            .into();
            assert!(
                self.execute_call(from, token, U256::ZERO, reset_data),
                "approve(spender, 0) reverted"
            );
        }

        let approve_data: Bytes =
            IERC20::approveCall::abi_encode(&IERC20::approveCall { spender, amount }).into();
        assert!(
            self.execute_call(from, token, U256::ZERO, approve_data),
            "approve reverted"
        );
    }

    /// Fund `holder` with `amount` of `token` at `slot`, then assert the read-back
    /// equals `amount` — catches a wrong balance-slot constant immediately.
    #[allow(dead_code)]
    pub fn fund_erc20_verified(
        &mut self,
        holder: Address,
        token: Address,
        slot: u64,
        amount: U256,
    ) {
        self.fund_erc20(holder, token, slot, amount);
        let got = self.erc20_balance(token, holder);
        assert_eq!(got, amount, "balance slot {slot} for {token} looks wrong");
    }

    /// Apply the swap's approval requirement from `from`, dispatching by kind:
    /// direct ERC-20, Permit2 two-step, or zero-first ERC-20.
    #[allow(dead_code)]
    pub fn apply_approval(
        &mut self,
        from: Address,
        prepared: &PreparedSwap,
        kind: super::ApprovalKind,
    ) {
        let Some(req) = &prepared.approval else {
            return;
        };
        let token = Address::from_word(req.token.token);
        match kind {
            super::ApprovalKind::Erc20 | super::ApprovalKind::ZeroFirst => {
                // `approve` already handles the zero-first hazard internally.
                self.approve(from, token, req.spender, req.min_allowance);
            }
            super::ApprovalKind::Permit2 => {
                // req.spender is Permit2; the UR is the Permit2 spender.
                self.permit2_approve(
                    from,
                    token,
                    req.spender,
                    super::dsl::UR,
                    req.min_allowance,
                    1_000_000_000_000u64,
                );
            }
        }
    }

    /// Assert the router holds no dust of any listed token after a swap
    /// (intermediates must fully pass through `ADDRESS_THIS`).
    #[allow(dead_code)]
    pub fn assert_zero_residue(&mut self, router: Address, tokens: &[Address]) {
        for &t in tokens {
            assert_eq!(
                self.erc20_balance(t, router),
                U256::ZERO,
                "router residue for {t} must be zero"
            );
        }
    }

    /// Grant `spender` (e.g. the Universal Router) a Permit2 allowance for
    /// `token` from `from`, using the two-step on-chain path (no EIP-712
    /// signature): first issue a standard ERC-20 approval to the Permit2
    /// contract, then call `Permit2.approve(token, spender, amount, expiration)`.
    ///
    /// `amount` must fit in `uint160` and `expiration` must fit in `uint48`.
    #[allow(dead_code)]
    pub fn permit2_approve(
        &mut self,
        from: Address,
        token: Address,
        permit2: Address,
        spender: Address,
        amount: U256,
        expiration: u64,
    ) {
        // Step 1: ERC-20 approve Permit2 to pull the token (use U256::MAX so
        // Permit2 can satisfy any downstream call without a re-approval).
        self.approve(from, token, permit2, U256::MAX);

        // Step 2: Permit2.approve(token, spender, amount as uint160, expiration as uint48).
        // U256→U160: use checked_from_limbs_slice (same pattern as production V3 encoder).
        // u64→U48: the limb slice has exactly one u64; checked_from_limbs_slice validates the
        // high bits are clear (i.e. value fits 48 bits).
        use alloy::sol_types::SolCall as _;
        let call = IPermit2::approveCall {
            token,
            spender,
            amount: alloy::primitives::aliases::U160::checked_from_limbs_slice(amount.as_limbs())
                .expect("amount must fit uint160 for Permit2.approve"),
            expiration: alloy::primitives::aliases::U48::checked_from_limbs_slice(&[expiration])
                .expect("expiration must fit uint48 for Permit2.approve"),
        };
        assert!(
            self.execute_call(from, permit2, U256::ZERO, call.abi_encode().into()),
            "Permit2.approve reverted"
        );
    }

    // ── transaction submission ───────────────────────────────────────────────

    /// Execute `tx` from `from` against the in-process EVM, commit the state
    /// change, and return `true` when the transaction did not revert.
    ///
    /// Gas price is pinned to `0` (`gas_price=0`) and `disable_base_fee` is
    /// set, so `effectiveGasPrice` is always `0` and no native-token balance
    /// is consumed by gas costs.
    pub fn submit(&mut self, from: Address, tx: &UnsignedTx) -> bool {
        self.execute_call(from, tx.to, tx.value, tx.data.clone())
    }

    // ── balance reads ─────────────────────────────────────────────────────────

    /// ERC-20 balance of `who` for `token`, read via a static call.
    pub fn erc20_balance(&mut self, token: Address, who: Address) -> U256 {
        use alloy::sol_types::SolCall as _;
        let call = IERC20::balanceOfCall { owner: who };
        let data: Bytes = IERC20::balanceOfCall::abi_encode(&call).into();
        let output = self.static_call(token, data);
        IERC20::balanceOfCall::abi_decode_returns(&output).unwrap_or(U256::ZERO)
    }

    /// Native-token (ETH) balance of `who`.
    pub fn native_balance(&mut self, who: Address) -> U256 {
        use revm::database_interface::Database as _;
        let db = self.db.as_mut().expect("db must be present");
        match db.basic(who) {
            Ok(Some(info)) => info.balance,
            _ => U256::ZERO,
        }
    }

    // ── snapshot / revert ─────────────────────────────────────────────────────

    /// Clone the current `CacheDB` state.
    ///
    /// Cheap: [`SharedBackend`] is `Arc`-backed, so only the dirty-state layer
    /// is cloned.  Pass the returned value to [`revert`] to roll back.
    pub fn snapshot(&self) -> ForkDb {
        self.db.as_ref().expect("db must be present").clone()
    }

    /// Restore to a previously snapped `CacheDB`.
    pub fn revert(&mut self, snap: ForkDb) {
        self.db = Some(snap);
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Low-level call helper used by both [`submit`] and [`approve`].
    fn execute_call(&mut self, from: Address, to: Address, value: U256, data: Bytes) -> bool {
        let db = self.db.take().expect("db must be present");

        let mut cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.chain_id = self.chain.0;
        cfg.disable_nonce_check = true;
        cfg.disable_base_fee = true;

        let tx_env = TxEnv {
            caller: from,
            kind: TxKind::Call(to),
            value,
            data,
            gas_limit: 10_000_000u64,
            gas_price: 0u128,
            ..TxEnv::default()
        };

        let block_env = self.block_env.clone();
        let ctx = Context::mainnet()
            .with_db(db)
            .with_cfg(cfg)
            .with_block(block_env);
        let mut evm = ctx.build_mainnet();

        let result = evm
            .transact_commit(tx_env)
            .expect("EVM transact_commit failed");

        // Reclaim the DB from the EVM context's journal.
        self.db = Some(evm.ctx.journaled_state.database);

        match result {
            revm::context::result::ExecutionResult::Success { .. } => true,
            revm::context::result::ExecutionResult::Revert { .. } => false,
            revm::context::result::ExecutionResult::Halt { .. } => false,
        }
    }

    /// Set an account's ETH balance directly in the `CacheDB`.
    ///
    /// Loads the existing account to preserve nonce and code_hash, then
    /// overwrites only the balance field.
    fn set_balance_in_db(db: &mut ForkDb, address: Address, balance: U256) {
        use revm::database_interface::Database as _;
        let account_info = db.basic(address).ok().flatten().unwrap_or(AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
        });

        db.insert_account_info(
            address,
            AccountInfo {
                balance,
                ..account_info
            },
        );
    }

    /// Execute a zero-value read-only call and return the raw return bytes.
    ///
    /// Non-committing: the call's state writes are discarded because
    /// [`ExecuteEvm::transact`] is used instead of `transact_commit`.  No DB
    /// clone is needed and the caller is not funded — `gas_price = 0` and
    /// `value = 0` produce zero upfront cost, and `disable_balance_check`
    /// suppresses the balance assertion for callers that carry no balance.
    fn static_call(&mut self, to: Address, data: Bytes) -> Bytes {
        let db = self.db.take().expect("db must be present");

        let static_caller = Address::repeat_byte(0xFF);

        let mut cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.chain_id = self.chain.0;
        cfg.disable_nonce_check = true;
        cfg.disable_base_fee = true;
        cfg.disable_balance_check = true;

        let tx_env = TxEnv {
            caller: static_caller,
            kind: TxKind::Call(to),
            value: U256::ZERO,
            data,
            gas_limit: 5_000_000u64,
            gas_price: 0u128,
            ..TxEnv::default()
        };

        let block_env = self.block_env.clone();
        let ctx = Context::mainnet()
            .with_db(db)
            .with_cfg(cfg)
            .with_block(block_env);
        let mut evm = ctx.build_mainnet();

        // transact() returns the result without applying state to the DB —
        // the read-through cache stays populated; the call's writes are dropped.
        let result = evm.transact(tx_env);

        // Reclaim the DB from the EVM context.
        self.db = Some(evm.ctx.journaled_state.database);

        match result {
            Ok(exec) => match exec.result {
                revm::context::result::ExecutionResult::Success { output, .. } => {
                    output.into_data()
                }
                _ => Bytes::new(),
            },
            _ => Bytes::new(),
        }
    }

    /// Read the ERC-20 allowance of `owner` for `spender` on `token`.
    fn call_erc20_allowance(&mut self, owner: Address, token: Address, spender: Address) -> U256 {
        use alloy::sol_types::SolCall as _;
        let call = IERC20::allowanceCall { owner, spender };
        let data: Bytes = IERC20::allowanceCall::abi_encode(&call).into();
        let output = self.static_call(token, data);
        IERC20::allowanceCall::abi_decode_returns(&output).unwrap_or(U256::ZERO)
    }
}
