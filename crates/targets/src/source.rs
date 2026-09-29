//! Reading `current.json` and the file it names. Everything here is time-independent, so a result
//! can be cached until the bytes on disk change.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use susq_contracts::{
    ContractError, check_header, from_json, target_content_sha256,
    v1::{Header, TargetFile, TargetPointer},
};

use crate::{
    error::{Document, RejectReason},
    targets::Targets,
    validate::validate_content,
};

/// Name of the pointer inside the targets directory.
pub(crate) const POINTER_FILE: &str = "current.json";

/// Size and modification time of a target file: enough to notice an in-place rewrite without rehashing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// The target file a pointer resolved to, stamped just before it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    path: PathBuf,
    stamp: Option<FileStamp>,
}

impl Resolved {
    /// True while the file on disk still looks exactly as it did when it was read.
    pub(crate) fn is_current(&self) -> bool {
        FileStamp::of(&self.path) == self.stamp
    }
}

/// Outcome of reading and statically verifying one version of `current.json`.
pub(crate) struct Evaluation {
    /// The file the pointer named (pointer form only, and only once the name was deemed safe).
    pub(crate) resolved: Option<Resolved>,
    /// Verified targets, or why they were refused.
    pub(crate) result: Result<Targets, RejectReason>,
}

/// Raw bytes of `current.json`.
pub(crate) fn read_pointer(path: &Path) -> Result<Vec<u8>, RejectReason> {
    fs::read(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => RejectReason::PointerMissing {
            path: path.to_path_buf(),
        },
        _ => RejectReason::PointerUnreadable {
            path: path.to_path_buf(),
            message: e.to_string(),
        },
    })
}

/// Parse `current.json`, follow it if it is a pointer, and run every time-independent check.
pub(crate) fn evaluate(dir: &Path, pointer: &[u8], slug: &str) -> Evaluation {
    match parse_current(pointer) {
        Err(reason) => Evaluation {
            resolved: None,
            result: Err(reason),
        },
        Ok(Current::File(file)) => Evaluation {
            resolved: None,
            result: verify(*file, POINTER_FILE, None, slug),
        },
        Ok(Current::Pointer(ptr)) => follow(dir, &ptr, slug),
    }
}

/// The two accepted shapes of `current.json`.
enum Current {
    Pointer(TargetPointer),
    File(Box<TargetFile>),
}

fn parse_current(bytes: &[u8]) -> Result<Current, RejectReason> {
    let unparseable = |message: String| RejectReason::PointerUnparseable { message };
    let text = std::str::from_utf8(bytes).map_err(|e| unparseable(e.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| unparseable(e.to_string()))?;
    if !value.is_object() {
        return Err(unparseable("not a JSON object".into()));
    }
    // `file` exists only on TargetPointer, and TargetFile parsing refuses unknown fields.
    if value.get("file").is_some() {
        from_json(text)
            .map(Current::Pointer)
            .map_err(|e| unparseable(e.to_string()))
    } else {
        parse_target(text, POINTER_FILE).map(|f| Current::File(Box::new(f)))
    }
}

fn follow(dir: &Path, ptr: &TargetPointer, slug: &str) -> Evaluation {
    let checked =
        check_doc_header(ptr.header.as_ref(), Document::Pointer).and_then(|()| safe_file_name(&ptr.file));
    if let Err(reason) = checked {
        return Evaluation {
            resolved: None,
            result: Err(reason),
        };
    }
    let path = dir.join(&ptr.file);
    let resolved = Resolved {
        stamp: FileStamp::of(&path),
        path,
    };
    let result = read_target(&resolved.path, &ptr.file)
        .and_then(|file| verify(file, &ptr.file, Some(&ptr.content_sha256), slug));
    Evaluation {
        resolved: Some(resolved),
        result,
    }
}

/// Accept only a single plain path component: no separators, `.`/`..`, roots or prefixes.
fn safe_file_name(name: &str) -> Result<(), RejectReason> {
    let mut parts = Path::new(name).components();
    let single_normal = matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None));
    if single_normal && !name.contains(['/', '\\']) {
        Ok(())
    } else {
        Err(RejectReason::UnsafeFileName {
            name: name.to_owned(),
        })
    }
}

fn read_target(path: &Path, name: &str) -> Result<TargetFile, RejectReason> {
    let bytes = fs::read(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => RejectReason::FileMissing {
            file: name.to_owned(),
        },
        _ => RejectReason::FileUnreadable {
            file: name.to_owned(),
            message: e.to_string(),
        },
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|e| RejectReason::FileUnparseable {
        file: name.to_owned(),
        message: e.to_string(),
    })?;
    parse_target(text, name)
}

fn parse_target(text: &str, name: &str) -> Result<TargetFile, RejectReason> {
    from_json(text).map_err(|e| RejectReason::FileUnparseable {
        file: name.to_owned(),
        message: e.to_string(),
    })
}

fn verify(
    file: TargetFile,
    source: &str,
    pointer_hash: Option<&str>,
    slug: &str,
) -> Result<Targets, RejectReason> {
    check_doc_header(file.header.as_ref(), Document::TargetFile)?;
    if let Some(pointer) = pointer_hash.filter(|p| *p != file.content_sha256) {
        return Err(RejectReason::PointerHashMismatch {
            pointer: pointer.to_owned(),
            file: file.content_sha256.clone(),
        });
    }
    let computed = target_content_sha256(&file);
    if computed != file.content_sha256 {
        return Err(RejectReason::ContentHashMismatch {
            declared: file.content_sha256.clone(),
            computed,
        });
    }
    if file.tournament_slug != slug {
        return Err(RejectReason::WrongTournament {
            got: file.tournament_slug.clone(),
            want: slug.to_owned(),
        });
    }
    validate_content(&file)?;
    Ok(Targets::new(file, source.to_owned()))
}

fn check_doc_header(header: Option<&Header>, doc: Document) -> Result<(), RejectReason> {
    check_header(header).map(|_| ()).map_err(|e| match e {
        ContractError::SchemaVersion { got, want } => RejectReason::SchemaVersion { doc, got, want },
        _ => RejectReason::MissingHeader { doc },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_are_safe() {
        safe_file_name("targets-20261008T160000Z-deadbeef.json").expect("plain name");
        for bad in [
            "",
            ".",
            "..",
            "../x.json",
            "/etc/passwd",
            "a/b.json",
            "a\\b.json",
            "./x.json",
            "x.json/",
        ] {
            assert_eq!(
                safe_file_name(bad),
                Err(RejectReason::UnsafeFileName { name: bad.into() }),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn non_object_json_is_an_unparseable_pointer() {
        for bytes in [&b"[]"[..], b"42", b"\xff\xfe", b"{"] {
            assert!(matches!(
                parse_current(bytes),
                Err(RejectReason::PointerUnparseable { .. })
            ));
        }
    }
}
