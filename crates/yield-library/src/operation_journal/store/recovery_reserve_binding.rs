//! Durable observation of one owner-bound recovery-reserve namespace.
//!
//! A binding records descriptor facts that were observed while the live volume lock and the
//! full physical reserve proof were held. It is restart evidence only: reopening still requires
//! a caller-supplied root capability, lock capability, and fresh descriptor validation.

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity};

use super::super::capacity_reserve_namespace::RESERVE_FILE_NAME;

pub(super) const TABLE_NAME: &str = "operation_journal_recovery_reserve_binding";
pub(super) const NAMESPACE_VERSION: u64 = 1;
const MAX_IDENTITY_BYTES: usize = 256;

pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_recovery_reserve_binding (
    volume_device TEXT NOT NULL PRIMARY KEY,
    profile_identity TEXT NOT NULL,
    root_identity TEXT NOT NULL,
    file_identity TEXT NOT NULL,
    allocation_unit INTEGER NOT NULL,
    protected_floor_bytes INTEGER NOT NULL,
    namespace_version INTEGER NOT NULL,
    namespace_name TEXT NOT NULL,
    owner_generation INTEGER NOT NULL,
    CHECK (allocation_unit > 0),
    CHECK (protected_floor_bytes = 268435456),
    CHECK (namespace_version = 1),
    CHECK (namespace_name = 'recovery-reserve.bin'),
    CHECK (owner_generation > 0)
)";

#[derive(Debug, Error, PartialEq)]
pub(crate) enum ReserveBindingError {
    #[error("recovery-reserve binding SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("recovery-reserve binding is missing for volume {0:?}")]
    Missing(VolumeIdentity),
    #[error("recovery-reserve binding conflicts with the live observation for volume {0:?}")]
    Conflict(VolumeIdentity),
    #[error("recovery-reserve binding is corrupt: {0}")]
    Corrupt(String),
    #[error("recovery-reserve binding identity does not match: {0}")]
    IdentityMismatch(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReserveBinding {
    volume: VolumeIdentity,
    profile_identity: String,
    root_identity: String,
    file_identity: String,
    allocation_unit: u64,
    protected_floor_bytes: u64,
    namespace_version: u64,
    namespace_name: String,
    owner_generation: u64,
}

impl ReserveBinding {
    pub(super) fn from_live(
        volume: VolumeIdentity,
        profile_identity: &str,
        root_identity: &str,
        file_identity: &str,
        allocation_unit: u64,
        owner_generation: u64,
    ) -> Result<Self, ReserveBindingError> {
        let binding = Self {
            volume,
            profile_identity: profile_identity.to_owned(),
            root_identity: root_identity.to_owned(),
            file_identity: file_identity.to_owned(),
            allocation_unit,
            protected_floor_bytes: PROTECTED_FREE_SPACE_FLOOR,
            namespace_version: NAMESPACE_VERSION,
            namespace_name: RESERVE_FILE_NAME.to_owned(),
            owner_generation,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(super) fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    pub(super) fn profile_identity(&self) -> &str {
        &self.profile_identity
    }

    pub(super) fn root_identity(&self) -> &str {
        &self.root_identity
    }

    pub(super) fn file_identity(&self) -> &str {
        &self.file_identity
    }

    pub(super) fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }

    pub(super) fn protected_floor_bytes(&self) -> u64 {
        self.protected_floor_bytes
    }

    pub(super) fn namespace_version(&self) -> u64 {
        self.namespace_version
    }

    pub(super) fn namespace_name(&self) -> &str {
        &self.namespace_name
    }

    pub(super) fn owner_generation(&self) -> u64 {
        self.owner_generation
    }

    fn validate(&self) -> Result<(), ReserveBindingError> {
        if self.volume.device == 0 {
            return Err(ReserveBindingError::Corrupt(
                "volume identity is zero".into(),
            ));
        }
        for (value, field) in [
            (&self.profile_identity, "profile identity"),
            (&self.root_identity, "root identity"),
            (&self.file_identity, "file identity"),
        ] {
            if value.is_empty() || value.len() > MAX_IDENTITY_BYTES {
                return Err(ReserveBindingError::Corrupt(format!(
                    "{field} is outside its bounded size"
                )));
            }
        }
        if self.allocation_unit == 0 {
            return Err(ReserveBindingError::Corrupt(
                "allocation unit is zero".into(),
            ));
        }
        if self.protected_floor_bytes != PROTECTED_FREE_SPACE_FLOOR {
            return Err(ReserveBindingError::Corrupt(
                "protected floor is not the fixed reserve floor".into(),
            ));
        }
        if self.namespace_version != NAMESPACE_VERSION {
            return Err(ReserveBindingError::Corrupt(
                "namespace version is unsupported".into(),
            ));
        }
        if self.namespace_name != RESERVE_FILE_NAME {
            return Err(ReserveBindingError::Corrupt(
                "namespace name is unsupported".into(),
            ));
        }
        if self.owner_generation == 0 {
            return Err(ReserveBindingError::Corrupt(
                "owner generation is zero".into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn read(
    connection: &Connection,
    volume: &VolumeIdentity,
) -> Result<Option<ReserveBinding>, ReserveBindingError> {
    let row = connection
        .query_row(
            &format!(
                "SELECT volume_device, profile_identity, root_identity, file_identity,
                        allocation_unit, protected_floor_bytes, namespace_version,
                        namespace_name, owner_generation
                 FROM {TABLE_NAME} WHERE volume_device = ?1"
            ),
            params![volume.device.to_string()],
            |row| {
                // Validate ALL dynamic fields while borrowed from this original statement.
                // A corrupt large later field must not make us copy an earlier large field.
                for (index, maximum, field) in [
                    (0, 20, "volume identity"),
                    (1, MAX_IDENTITY_BYTES, "profile identity"),
                    (2, MAX_IDENTITY_BYTES, "root identity"),
                    (3, MAX_IDENTITY_BYTES, "file identity"),
                    (7, RESERVE_FILE_NAME.len(), "namespace name"),
                ] {
                    if let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(index)? {
                        if bytes.is_empty() || bytes.len() > maximum {
                            return Ok(Err(ReserveBindingError::Corrupt(format!(
                                "{field} is outside its bounded size"
                            ))));
                        }
                    }
                }
                Ok(Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                )))
            },
        )
        .optional()?
        .transpose()?;
    let Some((
        volume_device,
        profile_identity,
        root_identity,
        file_identity,
        allocation_unit,
        protected_floor_bytes,
        namespace_version,
        namespace_name,
        owner_generation,
    )) = row
    else {
        return Ok(None);
    };
    let stored_device = parse_device(&volume_device, "volume_device")?;
    if stored_device != volume.device {
        return Err(ReserveBindingError::Corrupt(
            "binding lookup identity differs from its row".into(),
        ));
    }
    let binding = ReserveBinding {
        volume: VolumeIdentity {
            device: stored_device,
        },
        profile_identity,
        root_identity,
        file_identity,
        allocation_unit: parse_integer(allocation_unit, "allocation_unit")?,
        protected_floor_bytes: parse_integer(protected_floor_bytes, "protected_floor_bytes")?,
        namespace_version: parse_integer(namespace_version, "namespace_version")?,
        namespace_name,
        owner_generation: parse_integer(owner_generation, "owner_generation")?,
    };
    binding.validate()?;
    Ok(Some(binding))
}

pub(super) fn persist(
    connection: &Connection,
    binding: &ReserveBinding,
) -> Result<(), ReserveBindingError> {
    binding.validate()?;
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        if let Some(existing) = read(connection, binding.volume())? {
            if existing == *binding {
                return Ok(());
            }
            return Err(ReserveBindingError::Conflict(binding.volume.clone()));
        }
        connection.execute(
            &format!(
                "INSERT INTO {TABLE_NAME}
                 (volume_device, profile_identity, root_identity, file_identity,
                  allocation_unit, protected_floor_bytes, namespace_version,
                  namespace_name, owner_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            ),
            params![
                binding.volume.device.to_string(),
                binding.profile_identity,
                binding.root_identity,
                binding.file_identity,
                to_sql_integer(binding.allocation_unit, "allocation_unit")?,
                to_sql_integer(binding.protected_floor_bytes, "protected_floor_bytes")?,
                to_sql_integer(binding.namespace_version, "namespace_version")?,
                binding.namespace_name,
                to_sql_integer(binding.owner_generation, "owner_generation")?,
            ],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(ReserveBindingError::Sql(error))
            } else {
                Ok(())
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

/// Rebind an existing same-profile reserve to the strictly newer owner generation produced by a
/// clean-marker restart.  The identity namespace is immutable: only the generation column may
/// change, and an uncertain commit is returned as an error so the caller keeps recovery-only
/// ownership rather than claiming a clean handoff.
pub(super) fn rebind_owner_generation(
    connection: &Connection,
    expected: &ReserveBinding,
    new_generation: u64,
) -> Result<(), ReserveBindingError> {
    rebind_owner_generation_with_post_commit(connection, expected, new_generation, || Ok(()))
}

fn rebind_owner_generation_with_post_commit<AfterCommit>(
    connection: &Connection,
    expected: &ReserveBinding,
    new_generation: u64,
    after_commit: AfterCommit,
) -> Result<(), ReserveBindingError>
where
    AfterCommit: FnOnce() -> Result<(), ReserveBindingError>,
{
    expected.validate()?;
    if new_generation <= expected.owner_generation() {
        return Err(ReserveBindingError::IdentityMismatch(
            "owner generation must advance strictly during clean handoff".into(),
        ));
    }
    let stored_new_generation = i64::try_from(new_generation).map_err(|_| {
        ReserveBindingError::Corrupt("new owner generation exceeds SQLite INTEGER range".into())
    })?;
    let stored_old_generation = i64::try_from(expected.owner_generation()).map_err(|_| {
        ReserveBindingError::Corrupt("owner generation exceeds SQLite INTEGER range".into())
    })?;
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let Some(stored) = read(connection, expected.volume())? else {
            return Err(ReserveBindingError::Missing(expected.volume.clone()));
        };
        if stored != *expected {
            return Err(ReserveBindingError::IdentityMismatch(
                "stored reserve binding differs before generation rebind".into(),
            ));
        }
        let changed = connection.execute(
            &format!(
                "UPDATE {TABLE_NAME} SET owner_generation = ?1
                 WHERE volume_device = ?2
                   AND profile_identity = ?3
                   AND root_identity = ?4
                   AND file_identity = ?5
                   AND allocation_unit = ?6
                   AND protected_floor_bytes = ?7
                   AND namespace_version = ?8
                   AND namespace_name = ?9
                   AND owner_generation = ?10"
            ),
            params![
                stored_new_generation,
                expected.volume.device.to_string(),
                expected.profile_identity,
                expected.root_identity,
                expected.file_identity,
                to_sql_integer(expected.allocation_unit, "allocation_unit")?,
                to_sql_integer(expected.protected_floor_bytes, "protected_floor_bytes")?,
                to_sql_integer(expected.namespace_version, "namespace_version")?,
                expected.namespace_name,
                stored_old_generation,
            ],
        )?;
        if changed != 1 {
            return Err(ReserveBindingError::Conflict(expected.volume.clone()));
        }
        Ok(())
    })();
    match result {
        Ok(()) => match connection.execute_batch("COMMIT") {
            Ok(()) => after_commit(),
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(ReserveBindingError::Sql(error))
            }
        },
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

#[cfg(test)]
fn rebind_owner_generation_with_post_commit_error(
    connection: &Connection,
    expected: &ReserveBinding,
    new_generation: u64,
) -> Result<(), ReserveBindingError> {
    rebind_owner_generation_with_post_commit(connection, expected, new_generation, || {
        Err(ReserveBindingError::Corrupt(
            "injected post-commit acknowledgement uncertainty".into(),
        ))
    })
}

fn parse_device(value: &str, field: &str) -> Result<u64, ReserveBindingError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| ReserveBindingError::Corrupt(format!("{field} is not canonical u64 text")))?;
    if parsed == 0 || parsed.to_string() != value {
        return Err(ReserveBindingError::Corrupt(format!(
            "{field} is not canonical u64 text"
        )));
    }
    Ok(parsed)
}

fn parse_integer(value: i64, field: &str) -> Result<u64, ReserveBindingError> {
    u64::try_from(value).map_err(|_| ReserveBindingError::Corrupt(format!("{field} is negative")))
}

fn to_sql_integer(value: u64, field: &str) -> Result<i64, ReserveBindingError> {
    i64::try_from(value)
        .map_err(|_| ReserveBindingError::Corrupt(format!("{field} exceeds SQLite INTEGER")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn generation_rebind_updates_only_exact_matching_binding() {
        let connection = Connection::open_in_memory().expect("binding database");
        connection
            .execute_batch(CREATE_TABLE)
            .expect("binding schema");
        let binding = ReserveBinding::from_live(
            VolumeIdentity { device: 9 },
            "profile",
            "root",
            "file",
            4096,
            3,
        )
        .expect("binding");
        persist(&connection, &binding).expect("persist binding");
        rebind_owner_generation(&connection, &binding, 4).expect("rebind generation");
        assert_eq!(
            read(&connection, binding.volume())
                .unwrap()
                .unwrap()
                .owner_generation(),
            4
        );
        assert!(matches!(
            rebind_owner_generation(&connection, &binding, 5),
            Err(ReserveBindingError::IdentityMismatch(_))
        ));
        assert_eq!(
            read(&connection, binding.volume())
                .unwrap()
                .unwrap()
                .owner_generation(),
            4
        );
    }

    #[test]
    fn post_commit_rebind_error_keeps_generation_for_restart_reconciliation() {
        let directory = tempfile::tempdir().expect("binding directory");
        let database_path: PathBuf = directory.path().join("binding.sqlite3");
        let connection = Connection::open(&database_path).expect("binding database");
        connection
            .execute_batch(CREATE_TABLE)
            .expect("binding schema");
        let binding = ReserveBinding::from_live(
            VolumeIdentity { device: 10 },
            "profile",
            "root",
            "file",
            4096,
            5,
        )
        .expect("binding");
        persist(&connection, &binding).expect("persist binding");
        assert!(matches!(
            rebind_owner_generation_with_post_commit_error(&connection, &binding, 6),
            Err(ReserveBindingError::Corrupt(reason))
                if reason.contains("post-commit acknowledgement uncertainty")
        ));
        drop(connection);

        let reopened = Connection::open(&database_path).expect("reopen binding database");
        let rebound = read(&reopened, binding.volume())
            .expect("read rebound binding")
            .expect("rebound binding remains durable");
        assert_eq!(rebound.owner_generation(), 6);
        assert_eq!(rebound.profile_identity(), binding.profile_identity());
        assert_eq!(rebound.root_identity(), binding.root_identity());
        assert_eq!(rebound.file_identity(), binding.file_identity());
        assert_eq!(rebound.allocation_unit(), binding.allocation_unit());
        assert_eq!(
            rebound.protected_floor_bytes(),
            binding.protected_floor_bytes()
        );
        assert_eq!(rebound.namespace_version(), binding.namespace_version());
        assert_eq!(rebound.namespace_name(), binding.namespace_name());
    }
}
