//! What to do when a transfer's target already exists.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use file_dermaga_core::DateTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Download => "download",
            Direction::Upload => "upload",
        })
    }
}

/// Default action for existing targets (separately configurable for
/// downloads and uploads).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExistsAction {
    /// Ask the user (through the resolver).
    #[default]
    Ask,
    Overwrite,
    /// Overwrite if the source is newer.
    Newer,
    /// Overwrite if the sizes differ.
    SizeDiffers,
    /// Overwrite if the sizes differ or the source is newer.
    SizeDiffersOrNewer,
    /// Continue an interrupted transfer.
    Resume,
    Skip,
}

impl FromStr for ExistsAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "ask" => ExistsAction::Ask,
            "overwrite" => ExistsAction::Overwrite,
            "newer" => ExistsAction::Newer,
            "size" => ExistsAction::SizeDiffers,
            "size-or-newer" => ExistsAction::SizeDiffersOrNewer,
            "resume" => ExistsAction::Resume,
            "skip" => ExistsAction::Skip,
            other => return Err(format!("unknown action {other:?} (ask, overwrite, newer, size, size-or-newer, resume, skip)")),
        })
    }
}

/// Size and modification time; `None` when unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FileInfo {
    pub size: Option<u64>,
    pub mtime: Option<DateTime>,
}

#[derive(Clone, Debug)]
pub struct Conflict {
    pub direction: Direction,
    pub source_path: String,
    pub target_path: String,
    pub source: FileInfo,
    pub target: FileInfo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Overwrite,
    Resume,
    Skip,
    /// Transfer under a different target name (same directory).
    Rename(String),
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Asked for a decision when the action is `Ask`.
pub type Resolver<'a> = &'a mut (dyn FnMut(Conflict) -> BoxFuture<Decision> + Send);

/// Source newer than target? Unknown times count as newer (overwrite).
fn newer(src: &FileInfo, dst: &FileInfo) -> bool {
    match (src.mtime, dst.mtime) {
        (Some(s), Some(d)) => s.compare(&d) == std::cmp::Ordering::Greater,
        _ => true,
    }
}

/// Sizes differ? Unknown sizes count as different.
fn size_differs(src: &FileInfo, dst: &FileInfo) -> bool {
    match (src.size, dst.size) {
        (Some(a), Some(b)) => a != b,
        _ => true,
    }
}

/// The decision for an automatic action, or `None` if the user must decide.
pub fn decide(action: ExistsAction, src: &FileInfo, dst: &FileInfo) -> Option<Decision> {
    let ow = |yes: bool| if yes { Decision::Overwrite } else { Decision::Skip };
    match action {
        ExistsAction::Ask => None,
        ExistsAction::Overwrite => Some(Decision::Overwrite),
        ExistsAction::Skip => Some(Decision::Skip),
        ExistsAction::Resume => Some(Decision::Resume),
        ExistsAction::Newer => Some(ow(newer(src, dst))),
        ExistsAction::SizeDiffers => Some(ow(size_differs(src, dst))),
        ExistsAction::SizeDiffersOrNewer => Some(ow(size_differs(src, dst) || newer(src, dst))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fi(size: Option<u64>, day: Option<i64>) -> FileInfo {
        FileInfo { size, mtime: day.map(|d| DateTime::ymd(2024, 1, d)) }
    }

    #[test]
    fn automatic_decisions() {
        let (old, new) = (fi(Some(10), Some(1)), fi(Some(10), Some(2)));
        assert_eq!(decide(ExistsAction::Newer, &new, &old), Some(Decision::Overwrite));
        assert_eq!(decide(ExistsAction::Newer, &old, &new), Some(Decision::Skip));
        assert_eq!(decide(ExistsAction::Newer, &fi(Some(1), None), &old), Some(Decision::Overwrite));
        assert_eq!(decide(ExistsAction::SizeDiffers, &old, &new), Some(Decision::Skip));
        assert_eq!(decide(ExistsAction::SizeDiffers, &fi(Some(9), Some(1)), &old), Some(Decision::Overwrite));
        assert_eq!(decide(ExistsAction::SizeDiffers, &fi(None, Some(1)), &old), Some(Decision::Overwrite));
        assert_eq!(decide(ExistsAction::SizeDiffersOrNewer, &new, &old), Some(Decision::Overwrite));
        assert_eq!(decide(ExistsAction::SizeDiffersOrNewer, &old, &new), Some(Decision::Skip));
        assert_eq!(decide(ExistsAction::Ask, &old, &new), None);
        assert_eq!("size-or-newer".parse(), Ok(ExistsAction::SizeDiffersOrNewer));
        assert!("bogus".parse::<ExistsAction>().is_err());
    }
}
