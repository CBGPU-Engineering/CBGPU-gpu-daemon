//! Container entrypoint only; it never manages host containers or reservations.
use std::{
    fs, io,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
};
fn prepare(root: &Path) -> io::Result<()> {
    let run = root.join("run/sshd");
    fs::create_dir_all(run)?;
    let ssh = root.join("root/.ssh");
    if let Ok(meta) = fs::symlink_metadata(&ssh)
        && (!meta.is_dir() || meta.file_type().is_symlink())
    {
        return Err(io::Error::other("SSH directory must be a real directory"));
    }
    fs::create_dir_all(&ssh)?;
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700))?;
    let file = ssh.join("authorized_keys");
    if let Ok(meta) = fs::symlink_metadata(&file)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err(io::Error::other("SSH keys must be a regular file"));
    }
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&file)?;
    fs::set_permissions(file, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
fn main() -> io::Result<()> {
    if std::env::args().nth(1).as_deref() != Some("--run") {
        println!(
            "CBGPU container SSH/Jupyter entrypoint. Explicit --run is required; no host services are managed."
        );
        return Ok(());
    }
    prepare(Path::new("/"))?;
    let status = std::process::Command::new("/usr/sbin/sshd")
        .arg("-e")
        .status()?;
    if !status.success() {
        return Err(io::Error::other("Container SSH daemon failed"));
    }
    // Preserve the vendor's Bash environment initialization; application logic and
    // file/permission handling above are native. Jupyter itself is third-party.
    let error=std::process::Command::new("/bin/bash").args(["-e", "-c","source /etc/bash.bashrc; exec jupyter notebook --notebook-dir=/tf --ip 0.0.0.0 --no-browser --allow-root"]).exec();
    Err(error)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Root(std::path::PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn root() -> Root {
        let p = std::env::temp_dir().join(format!(
            "cbgpu-entrypoint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        Root(p)
    }
    #[test]
    fn preparation_preserves_existing_authorized_keys() {
        let root = root();
        prepare(&root.0).unwrap();
        let path = root.0.join("root/.ssh/authorized_keys");
        fs::write(&path, "fixture existing key").unwrap();
        prepare(&root.0).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"fixture existing key");
        assert_eq!(
            fs::metadata(root.0.join("root/.ssh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    #[test]
    fn symlinked_key_files_are_not_followed() {
        let root = root();
        prepare(&root.0).unwrap();
        let path = root.0.join("root/.ssh/authorized_keys");
        fs::remove_file(&path).unwrap();
        let original = root.0.join("original");
        fs::write(&original, "preserved").unwrap();
        std::os::unix::fs::symlink(&original, &path).unwrap();
        assert!(prepare(&root.0).is_err());
        assert_eq!(fs::read(original).unwrap(), b"preserved");
    }
}
