/// ABIs used internally in the executor
pub mod abi;
pub use abi::{
    patch_hardhat_console_selector, HardhatConsoleCalls, CONSOLE_ABI, HARDHAT_CONSOLE_ABI,
    HARDHAT_CONSOLE_ADDRESS,
};

/// Executor configuration
pub mod opts;

/// Executor databases
pub mod db;
pub use db::CacheDB;

/// Executor inspectors
pub mod inspector;

/// Executor builder
pub mod builder;
pub use builder::ExecutorBuilder;

/// Executor EVM spec identifiers
pub use revm::SpecId;

use bytes::Bytes;
use ethers::{
    abi::{Abi, Detokenize, RawLog, Tokenize},
    prelude::{decode_function_data, encode_function_data, Address, U256},
};
use eyre::Result;
use foundry_utils::IntoFunction;
use hashbrown::HashMap;
use inspector::{ExecutorState, LogCollector};
use revm::{
    db::{DatabaseCommit, DatabaseRef, EmptyDB},
    return_ok, Account, CreateScheme, Env, Return, TransactOut, TransactTo, TxEnv, EVM,
};
use std::{cell::RefCell, rc::Rc};

#[derive(thiserror::Error, Debug)]
pub enum EvmError {
    /// Error which occurred during execution of a transaction
    #[error("Execution reverted: {reason} (gas: {gas_used})")]
    Execution {
        status: Return,
        reason: String,
        gas_used: u64,
        logs: Vec<RawLog>,
        state_changeset: Option<HashMap<Address, Account>>,
    },
    /// Error which occurred during ABI encoding/decoding
    #[error(transparent)]
    AbiError(#[from] ethers::contract::AbiError),
    /// Any other error.
    #[error(transparent)]
    Eyre(#[from] eyre::Error),
}

/// The result of a call.
#[derive(Debug)]
pub struct CallResult<D: Detokenize> {
    /// The status of the call
    pub status: Return,
    /// The decoded result of the call
    pub result: D,
    /// The gas used for the call
    pub gas: u64,
    /// The logs emitted during the call
    pub logs: Vec<RawLog>,
    /// The changeset of the state.
    ///
    /// This is only present if the changed state was not committed to the database (i.e. if you
    /// used `call` and `call_raw` not `call_committing` or `call_raw_committing`).
    pub state_changeset: Option<HashMap<Address, Account>>,
}

/// The result of a raw call.
#[derive(Debug)]
pub struct RawCallResult {
    /// The status of the call
    pub status: Return,
    /// The raw result of the call
    pub result: Bytes,
    /// The gas used for the call
    pub gas: u64,
    /// The logs emitted during the call
    pub logs: Vec<RawLog>,
    /// The changeset of the state.
    ///
    /// This is only present if the changed state was not committed to the database (i.e. if you
    /// used `call` and `call_raw` not `call_committing` or `call_raw_committing`).
    pub state_changeset: Option<HashMap<Address, Account>>,
}

pub struct Executor<DB: DatabaseRef> {
    // Note: We do not store an EVM here, since we are really
    // only interested in the database. REVM's `EVM` is a thin
    // wrapper around spawning a new EVM on every call anyway,
    // so the performance difference should be negligible.
    //
    // Also, if we stored the VM here we would still need to
    // take `&mut self` when we are not committing to the database, since
    // we need to set `evm.env`.
    db: CacheDB<DB>,
    env: Env,
    // TODO: Here we are going to store information about the enabled inspectors, or just the
    // meta-inspector.
    // NOTE: It is important that the inspector gets a new state every time.
    //inspector: LogCollector,
}

impl<DB> Executor<DB>
where
    DB: DatabaseRef,
{
    pub fn new(inner_db: DB, env: Env) -> Self {
        Executor { db: CacheDB::new(inner_db), env }
    }

    /// Set the balance of an account.
    pub fn set_balance(&mut self, address: Address, amount: U256) {
        let mut account = self.db.basic(address);
        account.balance = amount;

        self.db.insert_cache(address, account);
    }

    /// Calls the `setUp()` function on a contract.
    pub fn setup(
        &mut self,
        address: Address,
    ) -> std::result::Result<(Return, Vec<RawLog>), EvmError> {
        let CallResult { status, logs, .. } = self.call_committing::<(), _, _>(
            Address::zero(),
            address,
            "setUp()",
            (),
            0.into(),
            None,
        )?;
        Ok((status, logs))
    }

    /// Performs a call to an account on the current state of the VM.
    ///
    /// The state after the call is persisted.
    pub fn call_committing<D: Detokenize, T: Tokenize, F: IntoFunction>(
        &mut self,
        from: Address,
        to: Address,
        func: F,
        args: T,
        value: U256,
        abi: Option<&Abi>,
    ) -> std::result::Result<CallResult<D>, EvmError> {
        let func = func.into();
        let calldata = Bytes::from(encode_function_data(&func, args)?.to_vec());
        let RawCallResult { result, status, gas, logs, .. } =
            self.call_raw_committing(from, to, calldata, value)?;
        match status {
            return_ok!() => {
                let result = decode_function_data(&func, result, false)?;
                Ok(CallResult { status, result, gas, logs, state_changeset: None })
            }
            _ => {
                let reason = foundry_utils::decode_revert(result.as_ref(), abi)
                    .unwrap_or_else(|_| format!("{:?}", status));
                Err(EvmError::Execution {
                    status,
                    reason,
                    gas_used: gas,
                    logs,
                    state_changeset: None,
                })
            }
        }
    }

    /// Performs a raw call to an account on the current state of the VM.
    ///
    /// The state after the call is persisted.
    pub fn call_raw_committing(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
    ) -> Result<RawCallResult> {
        let mut evm = EVM::new();
        evm.env = self.env.clone();
        evm.env.tx = TxEnv {
            caller: from,
            transact_to: TransactTo::Call(to),
            data: calldata,
            value,
            ..Default::default()
        };
        evm.database(&mut self.db);

        // Run the call
        let state = Rc::new(RefCell::new(ExecutorState::new()));
        let (status, out, gas, _) = evm.inspect_commit(LogCollector::new(state.clone()));
        let result = match out {
            TransactOut::Call(data) => data,
            _ => Bytes::default(),
        };
        let state = Rc::try_unwrap(state).expect("no inspector should be alive").into_inner();

        Ok(RawCallResult { status, result, gas, logs: state.logs, state_changeset: None })
    }

    /// Performs a call to an account on the current state of the VM.
    ///
    /// The state after the call is not persisted.
    pub fn call<D: Detokenize, T: Tokenize, F: IntoFunction>(
        &self,
        from: Address,
        to: Address,
        func: F,
        args: T,
        value: U256,
        abi: Option<&Abi>,
    ) -> std::result::Result<CallResult<D>, EvmError> {
        let func = func.into();
        let calldata = Bytes::from(encode_function_data(&func, args)?.to_vec());
        let RawCallResult { result, status, gas, logs, state_changeset } =
            self.call_raw(from, to, calldata, value)?;
        match status {
            return_ok!() => {
                let result = decode_function_data(&func, result, false)?;
                Ok(CallResult { status, result, gas, logs, state_changeset })
            }
            _ => {
                let reason = foundry_utils::decode_revert(result.as_ref(), abi)
                    .unwrap_or_else(|_| format!("{:?}", status));
                Err(EvmError::Execution { status, reason, gas_used: gas, logs, state_changeset })
            }
        }
    }

    /// Performs a raw call to an account on the current state of the VM.
    ///
    /// The state after the call is not persisted.
    pub fn call_raw(
        &self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
    ) -> Result<RawCallResult> {
        let mut evm = EVM::new();
        evm.env = self.env.clone();
        evm.env.tx = TxEnv {
            caller: from,
            transact_to: TransactTo::Call(to),
            data: calldata,
            value,
            ..Default::default()
        };
        evm.database(&self.db);

        // Run the call
        let state = Rc::new(RefCell::new(ExecutorState::new()));
        let (status, out, gas, state_changeset, _) =
            evm.inspect_ref(LogCollector::new(state.clone()));
        let result = match out {
            TransactOut::Call(data) => data,
            _ => Bytes::default(),
        };
        let state = Rc::try_unwrap(state).expect("no inspector should be alive").into_inner();

        Ok(RawCallResult {
            status,
            result,
            gas,
            logs: state.logs,
            state_changeset: Some(state_changeset),
        })
    }

    /// Deploys a contract and commits the new state to the underlying database.
    pub fn deploy(
        &mut self,
        from: Address,
        code: Bytes,
        value: U256,
    ) -> Result<(Address, Return, u64, Vec<RawLog>)> {
        let mut evm = EVM::new();

        evm.env = self.env.clone();
        evm.env.tx = TxEnv {
            caller: from,
            transact_to: TransactTo::Create(CreateScheme::Create),
            data: code,
            value,
            ..Default::default()
        };
        evm.database(&mut self.db);

        let state = Rc::new(RefCell::new(ExecutorState::new()));
        let (status, out, gas, _) = evm.inspect_commit(LogCollector::new(state.clone()));
        let addr = match out {
            TransactOut::Create(_, Some(addr)) => addr,
            // TODO: We should have better error handling logic in the test runner
            // regarding deployments in general
            TransactOut::Create(_, None) => eyre::bail!("deployment failed"),
            _ => unreachable!(),
        };
        let state = Rc::try_unwrap(state).expect("no inspector should be alive").into_inner();

        Ok((addr, status, gas, state.logs))
    }

    /// Check if a call to a test contract was successful
    pub fn is_success(
        &self,
        address: Address,
        status: Return,
        state_changeset: HashMap<Address, Account>,
        should_fail: bool,
    ) -> bool {
        let mut success = matches!(status, return_ok!());

        // Construct a new VM with the state changeset
        let mut db = CacheDB::new(EmptyDB());
        db.insert_cache(address, self.db.basic(address));
        db.commit(state_changeset);
        let executor = Executor::new(db, self.env.clone());

        if success {
            // Check if a DSTest assertion failed
            let call = executor.call::<bool, _, _>(
                Address::zero(),
                address,
                "failed()(bool)",
                (),
                0.into(),
                None,
            );

            if let Ok(CallResult { result: failed, .. }) = call {
                success = !failed;
            }
        }

        should_fail ^ success
    }
}