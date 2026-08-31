use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

use super::tree::{
    build_error, canonical_meta, guest_path, EntryMetadata, LogicalRootfs, NodeKind,
};

const MAX_RUNTIME_TEXT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GUEST_INIT_BYTES: u64 = 256 * 1024 * 1024;

impl LogicalRootfs {
    pub(super) fn create_base_structure(&mut self) -> Result<()> {
        for path in [
            "dev",
            "proc",
            "sys",
            "tmp",
            "run",
            "etc",
            "var",
            "var/tmp",
            "var/log",
            "workspace",
        ] {
            self.directory(&guest_path(path)?, EntryMetadata::canonical(0o755))?;
        }
        Ok(())
    }

    pub(super) fn install_guest_init(
        &mut self,
        source: &Path,
        expected_sha256: &str,
    ) -> Result<()> {
        let metadata = std::fs::symlink_metadata(source).map_err(|error| {
            build_error(format!(
                "Failed to inspect guest init {}: {error}",
                source.display()
            ))
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(build_error(format!(
                "Guest init is not a plain file: {}",
                source.display()
            )));
        }
        if metadata.len() > MAX_GUEST_INIT_BYTES {
            return Err(build_error(format!(
                "Guest init exceeds the {}-byte limit: {}",
                MAX_GUEST_INIT_BYTES,
                source.display()
            )));
        }
        let sbin = guest_path("sbin")?;
        // Direct artifacts have a fixed boot contract. If the image supplies a
        // merged-/usr symlink, resolution lands in its target; otherwise create
        // `/sbin` so every cache hit can launch the same `/sbin/init` path
        // without re-reading or mounting the filesystem.
        let install_dir = self.resolve_directory(&sbin, true)?.expect("created /sbin");
        let mut destination = install_dir;
        destination.push(b"init".to_vec());
        let mut open_options = std::fs::OpenOptions::new();
        open_options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = open_options.open(source).map_err(BoxError::IoError)?;
        let opened = file.metadata().map_err(BoxError::IoError)?;
        if !opened.is_file() || opened.len() != metadata.len() {
            return Err(build_error(format!(
                "Guest init changed while opening: {}",
                source.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                return Err(build_error(format!(
                    "Guest init identity changed while opening: {}",
                    source.display()
                )));
            }
        }
        let (spooled, sparse) = self.spool(&mut file, metadata.len())?;
        let actual_sha256 = sha256_file(&spooled)?;
        if actual_sha256 != expected_sha256 {
            return Err(build_error(format!(
                "Guest init changed after its cache identity was computed: {}",
                source.display()
            )));
        }
        self.replace_with_new(
            destination,
            NodeKind::Regular {
                source: spooled,
                size: metadata.len(),
                sparse,
            },
            canonical_meta(0o755),
            BTreeMap::new(),
        )?;
        Ok(())
    }

    pub(super) fn create_essential_files(&mut self) -> Result<()> {
        self.ensure_account_entries(
            "etc/passwd",
            &[
                ("root", "root:x:0:0:root:/root:/bin/sh"),
                ("nobody", "nobody:x:65534:65534:nobody:/:/bin/false"),
            ],
        )?;
        self.ensure_account_entries(
            "etc/group",
            &[("root", "root:x:0:"), ("nogroup", "nogroup:x:65534:")],
        )?;
        self.write_runtime_file("etc/hosts", b"127.0.0.1\tlocalhost\n::1\t\tlocalhost\n")?;
        self.write_runtime_file(
            "etc/resolv.conf",
            b"nameserver 8.8.8.8\nnameserver 8.8.4.4\n",
        )?;
        self.write_runtime_file(
            "etc/nsswitch.conf",
            b"passwd: files\ngroup: files\nhosts: files dns\n",
        )
    }

    pub(super) fn validate_boot_contract(&mut self) -> Result<()> {
        let init_path = guest_path("sbin/init")?;
        let resolved = self
            .resolve_path(&init_path, true, false)?
            .ok_or_else(|| build_error("Direct ext4 rootfs has no /sbin/init"))?;
        let entry = self
            .entries
            .get(&resolved)
            .ok_or_else(|| build_error("Direct ext4 rootfs init path is unresolved"))?;
        let node = self
            .nodes
            .get(&entry.node)
            .ok_or_else(|| build_error("Direct ext4 rootfs init inode is missing"))?;
        let NodeKind::Regular { size, .. } = &node.kind else {
            return Err(build_error(
                "Direct ext4 rootfs /sbin/init is not a regular file",
            ));
        };
        if *size == 0
            || node.meta.mode != 0o755
            || node.meta.uid != 0
            || node.meta.gid != 0
            || node.meta.mtime != (0, 0)
        {
            return Err(build_error(
                "Direct ext4 rootfs /sbin/init violates the canonical boot contract",
            ));
        }
        Ok(())
    }

    fn ensure_account_entries(&mut self, path: &str, required: &[(&str, &str)]) -> Result<()> {
        let existing = self.read_runtime_text(path)?.unwrap_or_default();
        let mut content = existing.clone();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        for (name, entry) in required {
            if !existing
                .lines()
                .any(|line| line.split(':').next() == Some(name))
            {
                content.push_str(entry);
                content.push('\n');
            }
        }
        self.write_runtime_file(path, content.as_bytes())
    }

    fn read_runtime_text(&mut self, path: &str) -> Result<Option<String>> {
        let path = guest_path(path)?;
        let Some(resolved) = self.resolve_path(&path, true, false)? else {
            return Ok(None);
        };
        let node = &self.nodes[&self.entries[&resolved].node];
        let NodeKind::Regular { source, size, .. } = &node.kind else {
            return Err(build_error("Runtime text path is not a regular file"));
        };
        if *size > MAX_RUNTIME_TEXT_BYTES {
            return Err(build_error("Runtime text file exceeds its byte limit"));
        }
        let mut bytes = Vec::with_capacity(*size as usize);
        File::open(source)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(BoxError::IoError)?;
        Ok(Some(String::from_utf8(bytes).unwrap_or_default()))
    }

    fn write_runtime_file(&mut self, path: &str, content: &[u8]) -> Result<()> {
        let path = guest_path(path)?;
        let resolved = self
            .resolve_path(&path, true, true)?
            .ok_or_else(|| build_error("Failed to resolve runtime file"))?;
        let mut reader = content;
        let (source, sparse) = self.spool(&mut reader, content.len() as u64)?;
        if let Some(entry) = self.entries.get(&resolved).copied() {
            let node = self.nodes.get_mut(&entry.node).expect("runtime file inode");
            if !matches!(node.kind, NodeKind::Regular { .. }) {
                return Err(build_error(
                    "Runtime file destination is not a regular file",
                ));
            }
            node.kind = NodeKind::Regular {
                source,
                size: content.len() as u64,
                sparse,
            };
            node.meta = canonical_meta(0o644);
        } else {
            self.replace_with_new(
                resolved,
                NodeKind::Regular {
                    source,
                    size: content.len() as u64,
                    sparse,
                },
                canonical_meta(0o644),
                BTreeMap::new(),
            )?;
        }
        Ok(())
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = File::open(path).map_err(BoxError::IoError)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(BoxError::IoError)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

impl EntryMetadata {
    fn canonical(mode: u16) -> Self {
        Self {
            meta: canonical_meta(mode),
            xattrs: BTreeMap::new(),
        }
    }
}
