use crate::error::PersistenceError;
use crate::types::ChangeSet;

use bdk_wallet::chain::Merge;
use bdk_wallet::WalletPersister;

use std::sync::{Arc, Mutex};

/// Definition of a wallet persistence implementation.
#[uniffi::export(with_foreign)]
pub trait Persistence: Send + Sync {
    /// Initialize the total aggregate `ChangeSet` for the underlying wallet.
    fn initialize(&self) -> Result<Arc<ChangeSet>, PersistenceError>;

    /// Persist a `ChangeSet` to the total aggregate changeset of the wallet.
    fn persist(&self, changeset: Arc<ChangeSet>) -> Result<(), PersistenceError>;
}

pub(crate) enum PersistenceType {
    Custom(Arc<dyn Persistence>),
    /// TALIA FORK: the wallet's aggregate changeset, held in memory only. Upstream's in-memory
    /// persister was an in-memory SQLite database; this fork builds no SQLite.
    Memory(Box<bdk_wallet::ChangeSet>),
}

/// Wallet backend implementations.
#[derive(uniffi::Object)]
pub struct Persister {
    pub(crate) inner: Mutex<PersistenceType>,
}

#[uniffi::export]
impl Persister {
    /// Create a new persister that keeps the wallet's changes in memory only.
    ///
    /// TALIA FORK: a plain in-memory changeset instead of an in-memory SQLite database. Nothing
    /// is written anywhere; the wallet is rebuilt from its descriptors and a fresh sync every
    /// time (decision D5). Still returns `Result`, as upstream's did.
    #[uniffi::constructor]
    pub fn new_in_memory() -> Result<Self, PersistenceError> {
        Ok(Self {
            inner: PersistenceType::Memory(Box::default()).into(),
        })
    }

    /// Use a native persistence layer.
    #[uniffi::constructor]
    pub fn custom(persistence: Arc<dyn Persistence>) -> Self {
        Self {
            inner: PersistenceType::Custom(persistence).into(),
        }
    }
}

impl WalletPersister for PersistenceType {
    type Error = PersistenceError;

    fn initialize(persister: &mut Self) -> Result<bdk_wallet::ChangeSet, Self::Error> {
        match persister {
            PersistenceType::Memory(changeset) => Ok(changeset.as_ref().clone()),
            PersistenceType::Custom(any) => any
                .initialize()
                .map(|changeset| changeset.as_ref().clone().into()),
        }
    }

    fn persist(persister: &mut Self, changeset: &bdk_wallet::ChangeSet) -> Result<(), Self::Error> {
        match persister {
            PersistenceType::Memory(aggregate) => {
                aggregate.merge(changeset.clone());
                Ok(())
            }
            PersistenceType::Custom(any) => {
                let ffi_changeset: ChangeSet = changeset.clone().into();
                any.persist(Arc::new(ffi_changeset))
            }
        }
    }
}
