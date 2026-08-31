//! Property tests: arbitrary namespaces — nested dirs,
//! name edge cases, hardlink webs, size distributions, holes, whiteouts —
//! must build to an image that passes `e2fsck -fn` and round-trips
//! byte-exactly through the reader.

use std::collections::HashMap;
use std::io::Read;

use mkext4::reader::Fs;
use mkext4::sink::{CheckingSink, VecSink};
use mkext4::{FsBuilder, InodeCount, InodeHandle, Meta, Options, SparseSeg, ROOT};
use proptest::prelude::*;

mod common;
use common::{Pattern, EPOCH};

/// One declared entry in a generated directory.
#[derive(Debug, Clone)]
enum GenNode {
    File {
        size: u64,
    },
    Sparse {
        head: u64,
        hole: u64,
        tail: u64,
    },
    Symlink {
        target_len: usize,
    },
    /// Hardlink to the most recently declared file, when one exists.
    Hardlink,
    Dir {
        entries: Vec<(String, GenNode)>,
        /// Declare fully, then remove the whole subtree (whiteout).
        removed: bool,
    },
}

fn name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z][a-z0-9._-]{0,18}",
        // Name-length edge: maximum 255 bytes.
        Just("n".repeat(255)),
        // Multi-byte UTF-8.
        Just("файл-ütf8-名前".to_string()),
        Just(".hidden".to_string()),
    ]
}

fn node_strategy() -> impl Strategy<Value = GenNode> {
    let leaf = prop_oneof![
        4 => (0u64..200_000).prop_map(|size| GenNode::File { size }),
        1 => Just(GenNode::File { size: 0 }),
        // Straddle the single-extent boundary now and then.
        1 => Just(GenNode::File { size: 32_768 * 4096 + 5 }),
        1 => (1u64..8192, 1u64..64, 0u64..8192)
            .prop_map(|(head, hole_blocks, tail)| GenNode::Sparse {
                head,
                hole: hole_blocks * 4096,
                tail,
            }),
        1 => (1usize..200).prop_map(|target_len| GenNode::Symlink { target_len }),
        1 => Just(GenNode::Hardlink),
    ];
    leaf.prop_recursive(3, 40, 8, |inner| {
        (
            proptest::collection::vec((name_strategy(), inner), 0..8),
            proptest::bool::weighted(0.15),
        )
            .prop_map(|(entries, removed)| GenNode::Dir { entries, removed })
    })
}

/// Expected state of one path after the build.
#[derive(Debug, Clone, PartialEq)]
enum Expect {
    File {
        size: u64,
        seed: u64,
        sparse: Option<(u64, u64, u64)>,
    },
    Symlink {
        target: Vec<u8>,
    },
    Dir,
}

#[derive(Clone, Copy)]
struct LastFile {
    handle: InodeHandle,
    size: u64,
    seed: u64,
    sparse: Option<(u64, u64, u64)>,
}

struct Builder {
    b: FsBuilder,
    fills: Vec<(InodeHandle, u64 /* data bytes */, u64 /* seed */)>,
    model: HashMap<String, Expect>,
    last_file: Option<LastFile>,
    seed: u64,
}

impl Builder {
    fn declare(&mut self, dir: InodeHandle, dir_path: &str, entries: &[(String, GenNode)]) {
        let mut used = std::collections::HashSet::new();
        for (name, node) in entries {
            if !used.insert(name.clone()) {
                continue; // duplicate names within a generated dir: skip
            }
            let path = format!("{dir_path}/{name}");
            let meta = Meta::new(0o644, 1000, 100, (EPOCH, 0));
            self.seed += 1;
            let seed = self.seed;
            match node {
                GenNode::File { size } => {
                    let h = self.b.file(dir, name, meta, *size).unwrap();
                    if *size > 0 {
                        self.fills.push((h, *size, seed));
                    }
                    self.model.insert(
                        path,
                        Expect::File {
                            size: *size,
                            seed,
                            sparse: None,
                        },
                    );
                    self.last_file = Some(LastFile {
                        handle: h,
                        size: *size,
                        seed,
                        sparse: None,
                    });
                }
                GenNode::Sparse { head, hole, tail } => {
                    let mut segs = vec![SparseSeg::Data(head * 4096)];
                    segs.push(SparseSeg::Hole(*hole));
                    if *tail > 0 {
                        segs.push(SparseSeg::Data(*tail));
                    }
                    let h = self.b.file_sparse(dir, name, meta, &segs).unwrap();
                    let data = head * 4096 + tail;
                    self.fills.push((h, data, seed));
                    let sparse = Some((head * 4096, *hole, *tail));
                    self.model.insert(
                        path,
                        Expect::File {
                            size: head * 4096 + hole + tail,
                            seed,
                            sparse,
                        },
                    );
                }
                GenNode::Symlink { target_len } => {
                    let target: String = "t".repeat(*target_len);
                    self.b.symlink(dir, name, &target, meta).unwrap();
                    self.model.insert(
                        path,
                        Expect::Symlink {
                            target: target.into_bytes(),
                        },
                    );
                }
                GenNode::Hardlink => {
                    if let Some(lf) = self.last_file {
                        self.b.hardlink(dir, name, lf.handle).unwrap();
                        self.model.insert(
                            path,
                            Expect::File {
                                size: lf.size,
                                seed: lf.seed,
                                sparse: lf.sparse,
                            },
                        );
                    }
                }
                GenNode::Dir { entries, removed } => {
                    let child = self.b.mkdir(dir, name, meta).unwrap();
                    self.declare(child, &path, entries);
                    if *removed {
                        self.b.remove(dir, name).unwrap();
                        // Drop the whole subtree from the model; hardlinks
                        // into the subtree from outside don't exist in
                        // this generator (last_file links stay in-scope),
                        // so removal is exact.
                        let prefix = format!("{path}/");
                        self.model
                            .retain(|k, _| k != &path && !k.starts_with(&prefix));
                        // Fills of dropped files must not be attempted.
                        self.last_file = None;
                    } else {
                        self.model.insert(path, Expect::Dir);
                    }
                }
            }
        }
    }
}

fn check_namespace(entries: Vec<(String, GenNode)>) {
    let size = 2u64 << 30;
    let mut opts = Options::new(size, [7; 16], EPOCH);
    opts.hash_seed = [11, 22, 33, 44];
    opts.inodes = InodeCount::Exact(4096);
    opts.journal_blocks = Some(1024);
    let b = FsBuilder::new(opts).unwrap();
    let mut builder = Builder {
        b,
        fills: Vec::new(),
        model: HashMap::new(),
        last_file: None,
        seed: 0,
    };
    builder.declare(ROOT, "", &entries);
    let Builder {
        b, fills, model, ..
    } = builder;

    let layout = b.seal().unwrap();
    let mut sink = CheckingSink::new(VecSink::default());
    let mut w = layout.writer(&mut sink).unwrap();
    for (h, bytes, seed) in &fills {
        // Fills of removed subtrees are rejected; skip them.
        let _ = w.fill(*h, &mut Pattern::new(*bytes, *seed));
    }
    w.finish().unwrap();
    let image = sink.finish(size).unwrap().buf.clone();

    // Oracle 1: e2fsck.
    common::assert_fsck_clean(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        &image,
        &format!("prop_{}", std::process::id()),
    );

    // Oracle 2: our reader, structurally verified, then model round-trip.
    let fs = Fs::open(&image[..]).unwrap();
    let issues = fs.verify().unwrap();
    assert!(issues.is_empty(), "{:?}", &issues[..issues.len().min(3)]);

    for (path, expect) in &model {
        let ino = fs.resolve(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let inode = fs.inode(ino).unwrap();
        match expect {
            Expect::Dir => {
                assert_eq!(inode.file_type(), mkext4::spec::inode::FileType::Dir)
            }
            Expect::Symlink { target } => {
                assert_eq!(&fs.symlink_target(ino).unwrap(), target, "{path}");
            }
            Expect::File { size, seed, sparse } => {
                assert_eq!(inode.size, *size, "{path}: size");
                let extents = fs.extents(ino, &inode).unwrap();
                let mut got = vec![0u8; *size as usize];
                fs.read_file_at(&inode, &extents, 0, &mut got).unwrap();
                let want = match sparse {
                    None => {
                        let mut v = Vec::new();
                        Pattern::new(*size, *seed).read_to_end(&mut v).unwrap();
                        v
                    }
                    Some((head, hole, tail)) => {
                        let mut v = Vec::new();
                        Pattern::new(head + tail, *seed)
                            .read_to_end(&mut v)
                            .unwrap();
                        let mut full = v[..*head as usize].to_vec();
                        full.resize(full.len() + *hole as usize, 0);
                        full.extend_from_slice(&v[*head as usize..]);
                        full
                    }
                };
                assert_eq!(got, want, "{path}: content");
            }
        }
    }
    // Reverse: no unexpected top-level entries beyond lost+found.
    for e in fs.read_dir(mkext4::spec::ROOT_INO).unwrap() {
        let name = String::from_utf8_lossy(&e.name).into_owned();
        if [".", "..", "lost+found"].contains(&name.as_str()) {
            continue;
        }
        assert!(
            model.contains_key(&format!("/{name}")),
            "unexpected root entry {name:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 8,
        max_shrink_iters: 64,
        .. ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_namespaces_roundtrip(
        entries in proptest::collection::vec((name_strategy(), node_strategy()), 0..10)
    ) {
        check_namespace(entries);
    }
}
