//! Generated path shapes pin the sandbox boundary, including shapes that
//! example-based tests have historically missed (#411).

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{self, OpenOptions};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use tempfile::TempDir;

use super::ToolSandbox;

#[derive(Clone, Debug)]
enum PathShape {
    PlainFile,
    NestedDirectory,
    LinkInside,
    LinkOutside,
    DanglingInside,
    DanglingOutside,
    LinkChain { hops: usize, dangling: bool },
    SymlinkedDirectory,
    DotDotMiddle,
    DotDotFinal,
    TrailingSlash,
    TrailingDot,
    NonUtf8Final,
    Fifo,
}

fn path_shapes() -> impl Strategy<Value = PathShape> {
    prop_oneof![
        Just(PathShape::PlainFile),
        Just(PathShape::NestedDirectory),
        Just(PathShape::LinkInside),
        Just(PathShape::LinkOutside),
        Just(PathShape::DanglingInside),
        Just(PathShape::DanglingOutside),
        (2usize..=3, any::<bool>())
            .prop_map(|(hops, dangling)| PathShape::LinkChain { hops, dangling }),
        Just(PathShape::SymlinkedDirectory),
        Just(PathShape::DotDotMiddle),
        Just(PathShape::DotDotFinal),
        Just(PathShape::TrailingSlash),
        Just(PathShape::TrailingDot),
        Just(PathShape::NonUtf8Final),
        Just(PathShape::Fifo),
    ]
}

struct Fixture {
    _temp: TempDir,
    allowed: PathBuf,
    outside: PathBuf,
    target: PathBuf,
}

impl Fixture {
    /// `None` means this host has no working `mkfifo`; only that generated
    /// case is discarded, as the invariant cannot be exercised there.
    fn build(shape: &PathShape) -> Option<Self> {
        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        fs::create_dir_all(allowed.join("real/nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(allowed.join("plain"), b"inside").unwrap();
        fs::write(allowed.join("real/leaf"), b"inside leaf").unwrap();
        fs::write(outside.join("victim"), b"outside sentinel").unwrap();

        let target = match shape {
            PathShape::PlainFile => allowed.join("plain"),
            PathShape::NestedDirectory => allowed.join("real/nested"),
            PathShape::LinkInside => {
                let path = allowed.join("link");
                symlink(allowed.join("plain"), &path).unwrap();
                path
            }
            PathShape::LinkOutside => {
                let path = allowed.join("link");
                symlink(outside.join("victim"), &path).unwrap();
                path
            }
            PathShape::DanglingInside => {
                let path = allowed.join("dangling");
                symlink(allowed.join("missing"), &path).unwrap();
                path
            }
            PathShape::DanglingOutside => {
                let path = allowed.join("dangling");
                symlink(outside.join("missing"), &path).unwrap();
                path
            }
            PathShape::LinkChain { hops, dangling } => {
                let final_target = if *dangling {
                    allowed.join("missing-chain-target")
                } else {
                    allowed.join("plain")
                };
                for index in (0..*hops).rev() {
                    let link = allowed.join(format!("chain-{index}"));
                    let next = if index + 1 == *hops {
                        final_target.clone()
                    } else {
                        allowed.join(format!("chain-{}", index + 1))
                    };
                    symlink(next, link).unwrap();
                }
                allowed.join("chain-0")
            }
            PathShape::SymlinkedDirectory => {
                symlink(allowed.join("real"), allowed.join("via")).unwrap();
                allowed.join("via/leaf")
            }
            PathShape::DotDotMiddle => allowed.join("real/../plain"),
            PathShape::DotDotFinal => allowed.join("real/.."),
            PathShape::TrailingSlash => {
                let mut bytes = allowed.join("plain").into_os_string().into_vec();
                bytes.push(b'/');
                PathBuf::from(OsString::from_vec(bytes))
            }
            PathShape::TrailingDot => allowed.join("plain/."),
            PathShape::NonUtf8Final => {
                let path = allowed.join(OsString::from_vec(vec![b'n', 0x80]));
                fs::write(&path, b"non utf8").unwrap();
                path
            }
            PathShape::Fifo => {
                let path = allowed.join("pipe");
                let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                // SAFETY: `name` is a live, NUL-terminated pathname and the
                // call does not retain the pointer.
                if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
                    return None;
                }
                path
            }
        };

        Some(Self {
            _temp: temp,
            allowed,
            outside,
            target,
        })
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, (String, Vec<u8>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let relative = path.strip_prefix(root).unwrap().to_path_buf();
        if metadata.file_type().is_symlink() {
            out.insert(
                relative,
                (
                    "symlink".into(),
                    fs::read_link(path).unwrap().as_os_str().as_bytes().to_vec(),
                ),
            );
        } else if metadata.is_dir() {
            out.insert(relative, ("dir".into(), Vec::new()));
            let mut children: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            children.sort();
            for child in children {
                visit(root, &child, out);
            }
        } else if metadata.is_file() {
            out.insert(relative, ("file".into(), fs::read(path).unwrap()));
        } else {
            out.insert(relative, ("special".into(), Vec::new()));
        }
    }

    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

/// The synchronous spelling of `file_write::write_no_follow`: the property
/// needs to accept non-UTF-8 paths, which the JSON tool call cannot represent.
/// `O_NONBLOCK` only keeps a generated FIFO from waiting for a reader; it does
/// not change path resolution or the `O_NOFOLLOW` boundary under test.
fn write_no_follow(path: &Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    use std::io::Write;
    options.open(path)?.write_all(b"property write")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn generated_writes_never_reach_outside_the_grant(shape in path_shapes()) {
        let Some(fixture) = Fixture::build(&shape) else { return Ok(()); };
        let sandbox = ToolSandbox::new().allow_write(&fixture.allowed);
        let allowed = fs::canonicalize(&fixture.allowed).unwrap();
        let before = snapshot(&fixture.outside);

        if let Ok(canonical) = sandbox.check_write(&fixture.target) {
            prop_assert!(canonical.starts_with(&allowed), "{shape:?} returned {canonical:?}");
            prop_assert!(!fs::symlink_metadata(&canonical).map(|m| m.file_type().is_symlink()).unwrap_or(false));
            prop_assert!(canonical.parent().is_some_and(Path::exists));
            let _ = write_no_follow(&canonical);
        }

        prop_assert_eq!(snapshot(&fixture.outside), before, "{:?} changed the outside tree", shape);
    }

    #[test]
    fn generated_reads_only_return_regular_files_inside_the_grant(shape in path_shapes()) {
        let Some(fixture) = Fixture::build(&shape) else { return Ok(()); };
        let sandbox = ToolSandbox::new().allow_read(&fixture.allowed);
        let allowed = fs::canonicalize(&fixture.allowed).unwrap();

        if let Ok(canonical) = sandbox.check_read(&fixture.target) {
            prop_assert!(canonical.starts_with(&allowed), "{shape:?} returned {canonical:?}");
            prop_assert!(fs::metadata(&canonical).unwrap().is_file(), "{shape:?} returned a non-file");
        }
    }
}
