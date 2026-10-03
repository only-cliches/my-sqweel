use super::*;
use anyhow::Result;
use std::{future::Future, pin::Pin, sync::Arc};

type StorageFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub(crate) trait ErasedStorage: Send + Sync {
    fn load_catalog(&self) -> StorageFuture<'_, Option<StorageCatalog>>;
    fn list_tables<'a>(&'a self, database: &'a str) -> StorageFuture<'a, Vec<String>>;
    fn load_table<'a>(
        &'a self,
        database: &'a str,
        table: &'a str,
    ) -> StorageFuture<'a, Option<TableState>>;
    fn scan_rows(&self, scan: RowScan) -> StorageFuture<'_, RowPage>;
    fn commit(&self, batch: StorageBatch) -> StorageFuture<'_, ()>;
}
impl<S: AsyncStorage> ErasedStorage for S {
    fn load_catalog(&self) -> StorageFuture<'_, Option<StorageCatalog>> {
        Box::pin(AsyncStorage::load_catalog(self))
    }
    fn list_tables<'a>(&'a self, database: &'a str) -> StorageFuture<'a, Vec<String>> {
        Box::pin(AsyncStorage::list_tables(self, database))
    }
    fn load_table<'a>(
        &'a self,
        database: &'a str,
        table: &'a str,
    ) -> StorageFuture<'a, Option<TableState>> {
        Box::pin(AsyncStorage::load_table(self, database, table))
    }
    fn scan_rows(&self, scan: RowScan) -> StorageFuture<'_, RowPage> {
        Box::pin(AsyncStorage::scan_rows(self, scan))
    }
    fn commit(&self, batch: StorageBatch) -> StorageFuture<'_, ()> {
        Box::pin(AsyncStorage::commit(self, batch))
    }
}
#[derive(Clone)]
pub struct CustomStorage(pub(crate) Arc<dyn ErasedStorage>);

/// Storage owned by one engine. Memory mode never opens database files.
pub enum Storage {
    Memory,
    RocksDb(std::path::PathBuf),
    Custom(CustomStorage),
}
impl Storage {
    pub fn custom<S: AsyncStorage>(backend: S) -> Self {
        Self::Custom(CustomStorage(Arc::new(backend)))
    }
}
