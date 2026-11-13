//! This device's endpoint key: random, kept in `identity.key` (32 raw
//! bytes) in the data directory. Independent of invites, so knowing a link
//! never lets anyone act as its author.

use iroh::SecretKey;
use std::path::Path;

const FILE: &str = "identity.key";

pub fn load_or_create() -> SecretKey {
    load_or_create_in(&crate::ffmpeg::data_dir())
}

/// Reads `dir/identity.key`, or creates it. On a write error the new key
/// is used for this session only.
pub fn load_or_create_in(dir: &Path) -> SecretKey {
    let path = dir.join(FILE);
    match std::fs::read(&path) {
        Ok(b) if b.len() == 32 => {
            restrict(&path);
            return SecretKey::from_bytes(b.as_slice().try_into().unwrap());
        }
        Ok(_) => tracing::warn!("{} is damaged, creating a new identity", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            // Unreadable but maybe valid: don't overwrite it.
            tracing::warn!("identity: {e}; using a temporary identity");
            return SecretKey::generate();
        }
    }
    let key = SecretKey::generate();
    if let Err(e) = save(dir, &key.to_bytes()) {
        tracing::warn!("identity: cannot save {}: {e}; using it for this session only", path.display());
    }
    key
}

fn save(dir: &Path, bytes: &[u8; 32]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp); // a leftover would keep its mode
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(FILE))
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Unix: only the owner may read the key.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path)
            && m.permissions().mode() & 0o077 != 0
            && let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("identity: cannot restrict {}: {e}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bibicord-identity-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn created_once_and_reloaded() {
        let dir = temp_dir("reload");
        let a = load_or_create_in(&dir);
        let b = load_or_create_in(&dir);
        assert_eq!(a.to_bytes(), b.to_bytes());
        assert_eq!(std::fs::read(dir.join(FILE)).unwrap(), a.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Only the key file is left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn damaged_file_is_replaced() {
        let dir = temp_dir("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), b"short").unwrap();
        let a = load_or_create_in(&dir);
        assert_eq!(std::fs::read(dir.join(FILE)).unwrap(), a.to_bytes());
        assert_eq!(load_or_create_in(&dir).to_bytes(), a.to_bytes());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn wide_permissions_are_fixed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let a = load_or_create_in(&dir);
        let path = dir.join(FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_or_create_in(&dir).to_bytes(), a.to_bytes());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
