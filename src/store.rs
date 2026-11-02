//! Unix private storage. The key protects against ciphertext-only disclosure;
//! an actor able to read both key and ciphertext can decrypt the store.
use crate::{Error, Result};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use fs2::FileExt;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}
// Deliberately no Debug implementation for secrets or the store.
pub struct Store {
    dir: PathBuf,
    key: Zeroizing<Vec<u8>>,
    _lock: File,
}
fn checked_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Validation("store directory must be absolute"));
    }
    let mut current = PathBuf::from("/");
    for part in path.components().skip(1) {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(Error::Validation("invalid store directory"));
        }
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(Error::Validation(
                    "store path contains a symlink or non-directory",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&current)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let m = fs::metadata(path)?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(Error::Validation(
            "store directory must be owned by current user with mode 0700",
        ));
    }
    Ok(())
}
use std::os::unix::fs::DirBuilderExt;
fn private_open(path: &Path, create: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
    {
        return Err(Error::Validation(
            "secret file must be a private regular file owned by current user",
        ));
    }
    Ok(f)
}
/// Atomic replacement; target symlinks are replaced, never followed.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| Error::Io(e.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
impl Store {
    /// Holds an exclusive lock until dropped, serializing complete CLI runs.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        if dir.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error::Validation(
                "store directory must be outside git repositories",
            ));
        }
        checked_dir(dir)?;
        let lock = private_open(&dir.join("store.lock"), true)?;
        lock.try_lock_exclusive().map_err(|_| {
            Error::Validation("store is busy; retry after the other process finishes")
        })?;
        let path = dir.join("credentials.key");
        let key = match private_open(&path, false) {
            Ok(f) => {
                let mut key = Zeroizing::new(Vec::new());
                f.take(33).read_to_end(&mut key)?;
                if key.len() != 32 {
                    return Err(Error::Crypto);
                }
                key
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // Never replace a missing key for an existing encrypted store.
                if dir.join("credentials.enc").exists() || dir.join("session.enc").exists() {
                    return Err(Error::Crypto);
                }
                let mut key = Zeroizing::new(vec![0; 32]);
                OsRng.fill_bytes(&mut key);
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)?;
                f.write_all(&key)?;
                f.sync_all()?;
                key
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            dir: dir.to_owned(),
            key,
            _lock: lock,
        })
    }
    pub fn save(&self, name: &str, context: &str, plain: &[u8]) -> Result<()> {
        if !matches!(name, "credentials.enc" | "session.enc") {
            return Err(Error::Validation("invalid secret name"));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| Error::Crypto)?;
        let mut nonce = [0; 12];
        OsRng.fill_bytes(&mut nonce);
        let aad = format!("cooperalianca-v1:{name}:{context}");
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::Crypto)?;
        let mut blob = b"COOPR1\0".to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend(encrypted);
        write_private(&self.dir.join(name), &blob)
    }
    pub fn load(&self, name: &str, context: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        if !matches!(name, "credentials.enc" | "session.enc") {
            return Err(Error::Validation("invalid secret name"));
        }
        let file = match private_open(&self.dir.join(name), false) {
            Ok(f) => f,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut blob = Vec::new();
        file.take(4 * 1024 * 1024 + 1).read_to_end(&mut blob)?;
        if blob.len() > 4 * 1024 * 1024 || blob.len() < 35 || !blob.starts_with(b"COOPR1\0") {
            return Err(Error::Crypto);
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| Error::Crypto)?;
        let aad = format!("cooperalianca-v1:{name}:{context}");
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&blob[7..19]),
                Payload {
                    msg: &blob[19..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::Crypto)?;
        Ok(Some(Zeroizing::new(plain)))
    }
    pub fn enroll(&self, base: &str, credentials: &Credentials) -> Result<()> {
        if credentials.username.trim().is_empty() || credentials.password.is_empty() {
            return Err(Error::Validation("username and password required"));
        }
        let bytes = Zeroizing::new(serde_json::to_vec(credentials).map_err(|_| Error::Parse)?);
        self.save("credentials.enc", base, &bytes)?;
        self.clear_session()?;
        Ok(())
    }
    pub fn credentials(&self, base: &str) -> Result<Credentials> {
        let bytes = self
            .load("credentials.enc", base)?
            .ok_or(Error::Unauthenticated)?;
        serde_json::from_slice(&bytes).map_err(|_| Error::Crypto)
    }
    pub fn clear_session(&self) -> Result<()> {
        match fs::remove_file(self.dir.join("session.enc")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    /// Import a one-line password file outside git. Delete only after a verified encrypted round trip.
    pub fn seed_from_file(&self, base: &str, username: &str, source: &Path) -> Result<()> {
        if fs::symlink_metadata(source)?.file_type().is_symlink() {
            return Err(Error::Validation("password source must not be a symlink"));
        }
        let source = fs::canonicalize(source)?;
        if source.ancestors().skip(1).any(|p| p.join(".git").exists())
            || source.starts_with(&self.dir)
        {
            return Err(Error::Validation(
                "password source must be outside git and store directories",
            ));
        }
        let mut file = private_open(&source, false)?;
        if file.metadata()?.len() > 65536 {
            return Err(Error::Validation("password file is too large"));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        (&mut file).take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 {
            return Err(Error::Validation("password file is too large"));
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        let password = std::str::from_utf8(&bytes)
            .map_err(|_| Error::Validation("password file must be UTF-8"))?
            .to_owned();
        let creds = Credentials {
            username: username.to_owned(),
            password,
        };
        self.enroll(base, &creds)?;
        let check = self.credentials(base)?;
        if check.password != creds.password || check.username != creds.username {
            return Err(Error::Crypto);
        }
        // Re-open without following links and verify inode before best-effort overwrite.
        let mut writable = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&source)?;
        let old = file.metadata()?;
        let now = writable.metadata()?;
        if old.ino() != now.ino() || old.dev() != now.dev() {
            return Err(Error::Validation("password source changed during import"));
        }
        let mut noise = Zeroizing::new(vec![0; now.len() as usize]);
        OsRng.fill_bytes(&mut noise);
        writable.seek(SeekFrom::Start(0))?;
        writable.write_all(&noise)?;
        writable.sync_all()?;
        fs::remove_file(&source)?;
        if source.try_exists()? {
            return Err(Error::Validation("could not delete source"));
        }
        Ok(())
    }
}
