//! Ref transactions, and the clone's identity, a ref only this clone has.

use super::Repo;
use crate::error::Error;
use crate::ids::ObjectId;

/// The ref naming this clone: a blob of random bytes, written once.
const CLONE_ID_REF: &str = "refs/code-review/clone-id";

impl Repo {
    /// Points every ref of `updates` at its object, creating or overwriting it, in one
    /// transaction: all of them move or none does. Of several updates of one ref, the last
    /// applies.
    ///
    /// # Errors
    ///
    /// Returns an error when git refuses the transaction.
    pub fn update_refs(&self, updates: &[(String, ObjectId)]) -> Result<(), Error> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for (index, (name, oid)) in updates.iter().enumerate() {
            // Git refuses a transaction that updates a ref twice.
            if updates
                .get(index + 1..)
                .is_some_and(|later| later.iter().any(|(later, _)| later == name))
            {
                continue;
            }
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
    pub fn delete_refs(&self, names: &[String]) -> Result<(), Error> {
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
        Self::run_command_with_input(
            self.command(),
            &["update-ref", "--no-deref", "--stdin", "-z"],
            input,
            &[0],
        )?;
        Ok(())
    }

    /// Returns this clone's identity, or `None` before [`Self::ensure_clone_id`] first made one.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run.
    pub fn clone_id(&self) -> Result<Option<ObjectId>, Error> {
        self.ref_target(CLONE_ID_REF)
    }

    /// Returns this clone's identity, making it on first use: a blob of random bytes that
    /// `refs/code-review/clone-id` names.
    ///
    /// Every worktree of a clone shares it, and a new clone at the same path, which has no such
    /// ref, gets a new one. The ref is only ever created, never moved: of two first uses at
    /// once, the second adopts the first's.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot write the blob or the ref.
    pub fn ensure_clone_id(&self) -> Result<ObjectId, Error> {
        if let Some(id) = self.clone_id()? {
            return Ok(id);
        }
        let blob = Self::run_command_with_input(
            self.command(),
            &["hash-object", "-w", "--stdin"],
            format!("code-review clone {}\n", crate::random::hex(64)).into_bytes(),
            &[0],
        )?;
        let blob = ObjectId::parse(String::from_utf8_lossy(&blob.stdout).trim())?;
        // `create SP <ref> NUL <new-oid> NUL` fails when the ref exists.
        let created = Self::run_command_with_input(
            self.command(),
            &["update-ref", "--stdin", "-z"],
            format!("create {CLONE_ID_REF}\0{blob}\0").into_bytes(),
            &[0],
        );
        match created {
            Ok(_) => Ok(blob),
            Err(err @ Error::CommandFailed { .. }) => self.clone_id()?.ok_or(err),
            Err(err) => Err(err),
        }
    }
}
