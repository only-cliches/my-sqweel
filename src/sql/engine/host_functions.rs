use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;

use super::MysqlColumnType;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSignature {
    pub args: Vec<MysqlColumnType>,
    pub returns: MysqlColumnType,
    pub deterministic: bool,
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;
type Scalar = Arc<dyn Fn(Vec<Value>) -> Result<Value> + Send + Sync>;
type AsyncScalar = Arc<dyn Fn(Vec<Value>) -> BoxFuture<Value> + Send + Sync>;
type AggregateInit = Arc<dyn Fn() -> Result<Value> + Send + Sync>;
type AggregateStep = Arc<dyn Fn(Value, Vec<Value>) -> Result<Value> + Send + Sync>;
type AggregateFinish = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;
type AsyncAggregateInit = Arc<dyn Fn() -> BoxFuture<Value> + Send + Sync>;
type AsyncAggregateStep = Arc<dyn Fn(Value, Vec<Value>) -> BoxFuture<Value> + Send + Sync>;
type AsyncAggregateFinish = Arc<dyn Fn(Value) -> BoxFuture<Value> + Send + Sync>;

pub(super) enum HostFunction {
    Scalar {
        name: String,
        signature: FunctionSignature,
        callback: Scalar,
    },
    AsyncScalar {
        name: String,
        signature: FunctionSignature,
        callback: AsyncScalar,
        timeout: Duration,
    },
    Aggregate {
        name: String,
        signature: FunctionSignature,
        init: AggregateInit,
        step: AggregateStep,
        finish: AggregateFinish,
    },
    AsyncAggregate {
        name: String,
        signature: FunctionSignature,
        init: AsyncAggregateInit,
        step: AsyncAggregateStep,
        finish: AsyncAggregateFinish,
        timeout: Duration,
    },
}

impl HostFunction {
    pub(super) fn is_aggregate(&self) -> bool {
        matches!(self, Self::Aggregate { .. } | Self::AsyncAggregate { .. })
    }

    pub(super) fn is_async(&self) -> bool {
        matches!(self, Self::AsyncScalar { .. } | Self::AsyncAggregate { .. })
    }

    pub(super) fn signature(&self) -> &FunctionSignature {
        match self {
            Self::Scalar { signature, .. }
            | Self::AsyncScalar { signature, .. }
            | Self::Aggregate { signature, .. }
            | Self::AsyncAggregate { signature, .. } => signature,
        }
    }

    pub(super) fn invoke_scalar(
        &self,
        args: Vec<Value>,
        runtime: &crate::runtime::EngineRuntime,
    ) -> Result<Value> {
        let arity = self.signature().args.len();
        ensure!(
            args.len() == arity,
            "host function expects {arity} arguments, got {}",
            args.len()
        );
        let args = coerce_args(args, &self.signature().args)?;
        let value = match self {
            Self::Scalar { name, callback, .. } => {
                crate::runtime::run_sync_callback(|| callback(args))
                    .with_context(|| format!("host function {name}"))
            }
            Self::AsyncScalar {
                name,
                callback,
                timeout,
                ..
            } => {
                let callback = callback.clone();
                let timeout = *timeout;
                let name = name.clone();
                runtime.run(async move {
                    tokio::time::timeout(timeout, callback(args))
                        .await
                        .map_err(|_| anyhow!("host function {name} timed out"))?
                })
            }
            _ => Err(anyhow!("host function is an aggregate")),
        }
        .with_context(|| format!("host scalar callback {} failed", self.name()))?;
        coerce_result(value, self.signature().returns)
    }

    pub(super) fn invoke_aggregate(
        &self,
        group_args: Vec<Vec<Value>>,
        runtime: &crate::runtime::EngineRuntime,
    ) -> Result<Value> {
        let signature = self.signature();
        let rows = group_args
            .into_iter()
            .map(|args| coerce_args(args, &signature.args))
            .collect::<Result<Vec<_>>>()?;
        let value = match self {
            Self::Aggregate {
                name,
                init,
                step,
                finish,
                ..
            } => crate::runtime::run_sync_callback(|| {
                let mut state =
                    init().with_context(|| format!("host aggregate {name} init failed"))?;
                for args in rows {
                    state = step(state, args)
                        .with_context(|| format!("host aggregate {name} step failed"))?;
                }
                finish(state).with_context(|| format!("host aggregate {name} finish failed"))
            })?,
            Self::AsyncAggregate {
                name,
                init,
                step,
                finish,
                timeout,
                ..
            } => {
                let timeout = *timeout;
                let name = name.clone();
                let timeout_name = name.clone();
                let init = init.clone();
                let step = step.clone();
                let finish = finish.clone();
                runtime.run(async move {
                    tokio::time::timeout(timeout, async move {
                        let mut state = init()
                            .await
                            .with_context(|| format!("host aggregate {name} init failed"))?;
                        for args in rows {
                            state = step(state, args)
                                .await
                                .with_context(|| format!("host aggregate {name} step failed"))?;
                        }
                        finish(state)
                            .await
                            .with_context(|| format!("host aggregate {name} finish failed"))
                    })
                    .await
                    .map_err(|_| anyhow!("host aggregate {timeout_name} timed out"))?
                })?
            }
            _ => return Err(anyhow!("host function is scalar")),
        };
        coerce_result(value, signature.returns)
    }

    fn name(&self) -> &str {
        match self {
            Self::Scalar { name, .. }
            | Self::AsyncScalar { name, .. }
            | Self::Aggregate { name, .. }
            | Self::AsyncAggregate { name, .. } => name,
        }
    }
}

#[derive(Default)]
pub struct HostFunctionRegistry {
    functions: HashMap<String, HostFunction>,
}

impl HostFunctionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_scalar<F>(
        &mut self,
        name: impl AsRef<str>,
        signature: FunctionSignature,
        callback: F,
    ) -> Result<()>
    where
        F: Fn(Vec<Value>) -> Result<Value> + Send + Sync + 'static,
    {
        let name = normalized_name(name.as_ref())?;
        self.insert(
            name.clone(),
            HostFunction::Scalar {
                name,
                signature,
                callback: Arc::new(callback),
            },
        )
    }

    pub fn register_async_scalar<F, Fut>(
        &mut self,
        name: impl AsRef<str>,
        signature: FunctionSignature,
        callback: F,
    ) -> Result<()>
    where
        F: Fn(Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        self.register_async_scalar_with_timeout(name, signature, DEFAULT_TIMEOUT, callback)
    }

    pub fn register_async_scalar_with_timeout<F, Fut>(
        &mut self,
        name: impl AsRef<str>,
        signature: FunctionSignature,
        timeout: Duration,
        callback: F,
    ) -> Result<()>
    where
        F: Fn(Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        let name = normalized_name(name.as_ref())?;
        self.insert(
            name.clone(),
            HostFunction::AsyncScalar {
                name,
                signature,
                callback: Arc::new(move |args| Box::pin(callback(args))),
                timeout,
            },
        )
    }

    pub fn register_aggregate<I, S, F>(
        &mut self,
        name: impl AsRef<str>,
        signature: FunctionSignature,
        init: I,
        step: S,
        finish: F,
    ) -> Result<()>
    where
        I: Fn() -> Result<Value> + Send + Sync + 'static,
        S: Fn(Value, Vec<Value>) -> Result<Value> + Send + Sync + 'static,
        F: Fn(Value) -> Result<Value> + Send + Sync + 'static,
    {
        let name = normalized_name(name.as_ref())?;
        self.insert(
            name.clone(),
            HostFunction::Aggregate {
                name,
                signature,
                init: Arc::new(init),
                step: Arc::new(step),
                finish: Arc::new(finish),
            },
        )
    }

    pub fn register_async_aggregate<I, IFut, S, SFut, F, FFut>(
        &mut self,
        name: impl AsRef<str>,
        signature: FunctionSignature,
        init: I,
        step: S,
        finish: F,
    ) -> Result<()>
    where
        I: Fn() -> IFut + Send + Sync + 'static,
        IFut: Future<Output = Result<Value>> + Send + 'static,
        S: Fn(Value, Vec<Value>) -> SFut + Send + Sync + 'static,
        SFut: Future<Output = Result<Value>> + Send + 'static,
        F: Fn(Value) -> FFut + Send + Sync + 'static,
        FFut: Future<Output = Result<Value>> + Send + 'static,
    {
        self.register_async_aggregate_with_timeout(
            name,
            signature,
            DEFAULT_TIMEOUT,
            init,
            step,
            finish,
        )
    }

    pub fn register_async_aggregate_with_timeout<I, IFut, S, SFut, F, FFut>(
        &mut self,
        name: impl AsRef<str>,
        signature: FunctionSignature,
        timeout: Duration,
        init: I,
        step: S,
        finish: F,
    ) -> Result<()>
    where
        I: Fn() -> IFut + Send + Sync + 'static,
        IFut: Future<Output = Result<Value>> + Send + 'static,
        S: Fn(Value, Vec<Value>) -> SFut + Send + Sync + 'static,
        SFut: Future<Output = Result<Value>> + Send + 'static,
        F: Fn(Value) -> FFut + Send + Sync + 'static,
        FFut: Future<Output = Result<Value>> + Send + 'static,
    {
        let name = normalized_name(name.as_ref())?;
        self.insert(
            name.clone(),
            HostFunction::AsyncAggregate {
                name,
                signature,
                init: Arc::new(move || Box::pin(init())),
                step: Arc::new(move |state, args| Box::pin(step(state, args))),
                finish: Arc::new(move |state| Box::pin(finish(state))),
                timeout,
            },
        )
    }

    pub(super) fn lookup(&self, name: &str) -> Option<&HostFunction> {
        self.functions.get(&name.to_ascii_uppercase())
    }

    pub(super) fn contains(&self, name: &str) -> bool {
        self.lookup(name).is_some()
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.functions.keys().map(String::as_str)
    }

    fn insert(&mut self, name: String, function: HostFunction) -> Result<()> {
        ensure!(
            !self.functions.contains_key(&name),
            "host function {name} is already registered"
        );
        self.functions.insert(name, function);
        Ok(())
    }
}

fn normalized_name(name: &str) -> Result<String> {
    let name = name.trim();
    ensure!(!name.is_empty(), "host function name must not be empty");
    ensure!(
        name.as_bytes()[0].is_ascii_alphabetic() || name.as_bytes()[0] == b'_',
        "host function name must begin with a letter or underscore"
    );
    ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "host function name must contain only letters, digits, and underscores"
    );
    Ok(name.to_ascii_uppercase())
}

fn coerce_args(args: Vec<Value>, types: &[MysqlColumnType]) -> Result<Vec<Value>> {
    ensure!(
        args.len() == types.len(),
        "host function expects {} arguments, got {}",
        types.len(),
        args.len()
    );
    args.into_iter()
        .zip(types)
        .map(|(value, ty)| coerce(value, *ty))
        .collect()
}

fn coerce_result(value: Value, ty: MysqlColumnType) -> Result<Value> {
    coerce(value, ty)
}

fn coerce(value: Value, ty: MysqlColumnType) -> Result<Value> {
    super::eval::cast_json_value(value, mysql_type_name(ty))
}

pub(super) fn mysql_type_name(ty: MysqlColumnType) -> &'static str {
    match ty {
        MysqlColumnType::Null => "null",
        MysqlColumnType::TinyInt
        | MysqlColumnType::SmallInt
        | MysqlColumnType::Integer
        | MysqlColumnType::BigInt
        | MysqlColumnType::Year
        | MysqlColumnType::Bit => "signed",
        MysqlColumnType::Float | MysqlColumnType::Double => "double",
        MysqlColumnType::Decimal => "decimal",
        MysqlColumnType::Date => "date",
        MysqlColumnType::Time => "time",
        MysqlColumnType::DateTime => "datetime",
        MysqlColumnType::Timestamp => "timestamp",
        MysqlColumnType::Char
        | MysqlColumnType::Enum
        | MysqlColumnType::VarChar
        | MysqlColumnType::Text => "char",
        MysqlColumnType::Binary
        | MysqlColumnType::VarBinary
        | MysqlColumnType::Blob
        | MysqlColumnType::MediumBlob
        | MysqlColumnType::LongBlob => "binary",
        MysqlColumnType::Json => "json",
    }
}
