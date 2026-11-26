//! Ref transactions in a private repository.

use super::{PrivateRepo, Repo};
use crate::error::Error;
use crate::ids::ObjectId;

impl PrivateRepo {
    /// Points every ref of `updates`, each named once, at its object, creating or overwriting
    /// it, in one transaction: all of them move or none does.
    ///
    /// # Errors
    ///
    /// Returns an error when git refuses the transaction, as for an object that is not here.
    pub(crate) fn update_refs(&self, updates: &[(String, ObjectId)]) -> Result<(), Error> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for (name, oid) in updates {
            // `update SP <ref> NUL <new-oid> NUL <old-oid> NUL`, with no old value to verify.
            input.extend_from_slice(format!("update {name}\0{oid}\0\0").as_bytes());
        }
        self.ref_transaction(input)
    }

    /// Deletes every ref of `names` that exists, in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when git refuses the transaction.
    pub(crate) fn delete_refs(&self, names: &[String]) -> Result<(), Error> {
        if names.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for name in names {
            // `delete SP <ref> NUL <old-oid> NUL`, with no old value to verify.
            input.extend_from_slice(format!("delete {name}\0\0").as_bytes());
        }
        self.ref_transaction(input)
    }

    /// Runs `git update-ref --stdin -z` on the NUL-separated commands of `input`.
    fn ref_transaction(&self, input: Vec<u8>) -> Result<(), Error> {
        let mut command = self.repo().command();
        super::durable(&mut command);
        Repo::run_command_with_input(
            command,
            &["update-ref", "--no-deref", "--stdin", "-z"],
            input,
            &[0],
        )?;
        Ok(())
    }
}
