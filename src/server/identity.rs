//! Durable installation identity, resolved before the device actor or HTTP server starts.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::Path;

use uuid::Uuid;

pub(super) fn load_or_create_instance_id(data_dir: &Path) -> Result<String, String> {
    load_or_create_with(data_dir, Uuid::new_v4)
}

fn load_or_create_with(data_dir: &Path, generate: impl FnOnce() -> Uuid) -> Result<String, String> {
    fs::create_dir_all(data_dir).map_err(|error| {
        format!(
            "failed to create identity data directory {}: {error}",
            data_dir.display()
        )
    })?;
    let path = data_dir.join("instance-id");
    match fs::read_to_string(&path) {
        Ok(contents) => {
            return Uuid::parse_str(contents.trim())
                .map(|id| id.hyphenated().to_string())
                .map_err(|error| {
                    format!(
                        "invalid server instance identity {}: {error}",
                        path.display()
                    )
                });
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to read server instance identity {}: {error}",
                path.display()
            ));
        }
    }

    let id = generate().hyphenated().to_string();
    // A competing creation (including a dangling symlink) must never be overwritten.
    let mut file = OpenOptions::new().write(true).create_new(true).open(&path)
        .map_err(|error| format!("failed to create server instance identity {} (another server may own this data directory): {error}", path.display()))?;
    writeln!(file, "{id}")
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            format!(
                "failed to persist server instance identity {}: {error}",
                path.display()
            )
        })?;
    super::sync_directory(data_dir)?;
    Ok(id)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "identity persistence tests should fail fast"
)]
mod tests {
    use super::*;

    const FIRST: &str = "7f7fb259-89ef-49c2-a545-40ecf8d63e22";
    const SECOND: &str = "0b106ada-8525-47ef-8cfe-b9f81b36352a";

    fn directory(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ebc-identity-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("test directory");
        path
    }

    #[test]
    fn new_identity_is_canonical_v4_and_stable_without_rewrite() {
        let directory = directory("fresh");
        let id = load_or_create_instance_id(&directory).expect("create identity");
        let uuid = Uuid::parse_str(&id).expect("UUID");
        assert_eq!(uuid.get_version_num(), 4);
        assert_eq!(id, uuid.hyphenated().to_string());
        let path = directory.join("instance-id");
        assert_eq!(
            fs::read_to_string(&path).expect("read identity"),
            format!("{id}\n")
        );
        let modified = fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert_eq!(
            load_or_create_with(&directory, || panic!("must not regenerate")).expect("reload"),
            id
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("metadata")
                .modified()
                .expect("mtime"),
            modified
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn upgrade_preserves_existing_data_and_restored_identity() {
        let directory = directory("upgrade");
        let snapshot = crate::core::AuthoritativeSnapshot::default();
        super::super::Persistence::new(&directory)
            .expect("persistence")
            .save_metadata(&snapshot)
            .expect("session metadata");
        let session = fs::read(directory.join("session.json")).expect("session");
        let id = load_or_create_with(&directory, || Uuid::parse_str(FIRST).expect("UUID"))
            .expect("upgrade");
        assert_eq!(id, FIRST);
        assert_eq!(
            fs::read(directory.join("session.json")).expect("session"),
            session
        );
        let restored = directory.with_extension("restored");
        fs::create_dir(&restored).expect("restore directory");
        fs::copy(directory.join("instance-id"), restored.join("instance-id"))
            .expect("restore identity");
        assert_eq!(
            load_or_create_instance_id(&restored).expect("restored identity"),
            id
        );
        fs::remove_dir_all(restored).expect("cleanup restore");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn separate_directories_preserve_independent_generated_ids() {
        for expected in [FIRST, SECOND] {
            let directory = directory("independent");
            assert_eq!(
                load_or_create_with(&directory, || Uuid::parse_str(expected).expect("UUID"))
                    .expect("create"),
                expected
            );
            assert_eq!(
                load_or_create_instance_id(&directory).expect("reload"),
                expected
            );
            fs::remove_dir_all(directory).expect("cleanup");
        }
    }

    #[test]
    fn malformed_empty_truncated_and_unreadable_ids_are_preserved() {
        let directory = directory("invalid");
        let path = directory.join("instance-id");
        for contents in [b"not-a-uuid".as_slice(), b"", b"7f7fb259-89ef", b"\xff"] {
            fs::write(&path, contents).expect("invalid file");
            let error = load_or_create_with(&directory, || panic!("must not regenerate"))
                .expect_err("invalid identity rejected");
            assert!(error.contains("instance identity"), "{error}");
            assert_eq!(fs::read(&path).expect("read unchanged file"), contents);
        }
        fs::remove_file(&path).expect("remove invalid file");
        fs::create_dir(&path).expect("unreadable identity path");
        assert!(
            load_or_create_with(&directory, || panic!("must not generate"))
                .expect_err("read fails")
                .contains("failed to read")
        );
        assert!(path.is_dir());
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn accepted_uuid_representation_is_canonicalized_without_rewrite() {
        let directory = directory("canonical");
        let bytes = format!("  {}\r\n", FIRST.to_uppercase());
        fs::write(directory.join("instance-id"), &bytes).expect("existing UUID");
        assert_eq!(
            load_or_create_with(&directory, || panic!("must not regenerate")).expect("load"),
            FIRST
        );
        assert_eq!(
            fs::read_to_string(directory.join("instance-id")).expect("read original"),
            bytes
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn racing_creation_fails_without_clobbering_winner() {
        let directory = directory("race");
        let error = load_or_create_with(&directory, || {
            fs::write(directory.join("instance-id"), format!("{FIRST}\n"))
                .expect("competing creation");
            Uuid::parse_str(SECOND).expect("UUID")
        })
        .expect_err("creation race fails");
        assert!(error.contains("failed to create"), "{error}");
        assert_eq!(
            fs::read_to_string(directory.join("instance-id")).expect("winner"),
            format!("{FIRST}\n")
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn unusable_data_directory_fails_without_ephemeral_identity() {
        let directory = directory("io-error");
        let path = directory.join("file");
        fs::write(&path, b"existing data").expect("file instead of directory");
        assert!(
            load_or_create_with(&path, || panic!("must not generate"))
                .expect_err("creation fails")
                .contains("identity data directory")
        );
        assert_eq!(fs::read(&path).expect("unchanged file"), b"existing data");
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
