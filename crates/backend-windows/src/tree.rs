//! Turns NTFS `$MFT` record facts into walk items: subtree scoping, path building,
//! hard links, reparse classification and file metadata.
//!
//! Portable: nothing here touches a Win32 handle. The MFT reader fills [`RecordFacts`]
//! and the caller supplies the UTF-16 to path-component conversion.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::mem::size_of;

use dedupe_core::model::attrs;
use dedupe_core::path::RawPath;
use dedupe_core::traits::{EntryKind, FileMeta, WalkItem};

use crate::filetime::{ctime_ns, mtime_ns};

/// NTFS reserves records 0..=23 for metafiles such as `$MFT` and `$Extend`.
const METAFILE_LAST_RECORD: u64 = 23;
/// The NTFS root directory. It is the only reserved record that is walked.
const ROOT_DIR_RECORD: u64 = 5;
/// NTFS file references carry a 48-bit record number above a 16-bit sequence number.
const RECORD_NUMBER_MASK: u64 = (1 << 48) - 1;
/// Directory nesting limit. Corrupt data cannot build deeper paths than this.
const MAX_DEPTH: usize = 4096;
/// Fixed estimate of the per-record bookkeeping cost used for the memory budget.
const RECORD_OVERHEAD_BYTES: usize = 128;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
/// Same bits as `backend-generic` maps to `attrs::PLACEHOLDER` on Windows.
const PLACEHOLDER_ATTRIBUTES: u32 =
    FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

/// Microsoft reparse-tag bit: the reparse point names another file system object. std (and
/// so walkdir) treats exactly these as symlinks on Windows. Cloud, dedup and WOF tags do not
/// carry it, so those entries stay regular. Unconfirmed against real volumes; the Windows
/// self-check test verifies the classification.
const IO_REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;
#[cfg(test)]
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
#[cfg(test)]
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// Which `$FILE_NAME` namespace a name was stored under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameSpace {
    Posix,
    Win32,
    Dos,
    Win32AndDos,
}

/// One `$FILE_NAME` attribute: a link from `parent` to this record under `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameFact {
    /// Record number of the directory that holds this name.
    pub parent: u64,
    pub name: Vec<u16>,
    pub namespace: NameSpace,
}

/// Everything the walk needs from one MFT record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordFacts {
    pub record: u64,
    pub sequence: u16,
    pub in_use: bool,
    pub is_dir: bool,
    /// NTFS file attribute flags from `$STANDARD_INFORMATION`.
    pub attrs: u32,
    pub reparse_tag: Option<u32>,
    /// Last write time, `FILETIME` ticks.
    pub mtime_ft: u64,
    /// Creation time, `FILETIME` ticks.
    pub ctime_ft: u64,
    pub size: u64,
    pub names: Vec<NameFact>,
}

/// The in-memory index would exceed its budget. The caller should fall back to the walker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub limit_bytes: usize,
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MFT index exceeded its {} byte memory budget",
            self.limit_bytes
        )
    }
}

impl Error for BudgetExceeded {}

/// Records under one root, indexed for a single walk.
pub struct VolumeTree {
    root: u64,
    budget_bytes: usize,
    used_bytes: usize,
    /// Ordered by record number so child lists, and therefore the walk, are deterministic.
    records: BTreeMap<u64, RecordFacts>,
}

impl VolumeTree {
    pub fn new(root_record: u64, mem_budget: usize) -> Self {
        Self {
            root: root_record,
            budget_bytes: mem_budget,
            used_bytes: 0,
            records: BTreeMap::new(),
        }
    }

    /// Builds a tree from `facts` with the same filtering and budget check as [`Self::push`].
    ///
    /// The first record that breaks the budget fails the whole build, so no partial tree escapes.
    pub fn from_facts(
        root_record: u64,
        mem_budget: usize,
        facts: impl IntoIterator<Item = RecordFacts>,
    ) -> Result<Self, BudgetExceeded> {
        let mut tree = Self::new(root_record, mem_budget);
        for record in facts {
            tree.push(record)?;
        }
        Ok(tree)
    }

    /// Stores one record. Unused records, metafiles, and DOS-only names are dropped here,
    /// so they never reach the index.
    ///
    /// Returns [`BudgetExceeded`] without storing the record when the estimate would pass
    /// the budget.
    pub fn push(&mut self, mut facts: RecordFacts) -> Result<(), BudgetExceeded> {
        if !facts.in_use || is_system_metafile(facts.record) {
            return Ok(());
        }
        // A DOS 8.3 name is a second spelling of a link that already has a Win32 name.
        facts.names.retain(|name| name.namespace != NameSpace::Dos);
        if facts.names.is_empty() {
            return Ok(());
        }
        let cost = record_cost(&facts);
        let replaced = self.records.get(&facts.record).map_or(0, record_cost);
        // Saturating: an overflowing estimate is over any budget anyway.
        let used = self
            .used_bytes
            .saturating_sub(replaced)
            .saturating_add(cost);
        if used > self.budget_bytes {
            return Err(BudgetExceeded {
                limit_bytes: self.budget_bytes,
            });
        }
        self.used_bytes = used;
        self.records.insert(facts.record, facts);
        Ok(())
    }

    /// Stored records in ascending record-number order, as [`Self::push`] kept them.
    pub fn records_iter(&self) -> impl ExactSizeIterator<Item = &RecordFacts> {
        self.records.values()
    }

    /// Drops a stored record and releases its estimated cost from the budget.
    ///
    /// Does nothing when the record is not stored.
    pub fn remove(&mut self, record: u64) {
        if let Some(facts) = self.records.remove(&record) {
            self.used_bytes = self.used_bytes.saturating_sub(record_cost(&facts));
        }
    }

    /// Visits every walk item under the root without collecting them.
    ///
    /// Order: the root directory first, then every other directory in path byte order (a
    /// parent path is a strict prefix of its children's, so parents come first), then every
    /// file in ascending record order, with each file's names in stored order.
    ///
    /// A path is built only when its item is visited. The directory paths are the one cache
    /// kept during emission: the file phase needs them to build child paths. A directory item
    /// gets a clone of its cached path, dropped right after the visit, because the visitor
    /// takes ownership. Each file's facts are dropped once its names are visited.
    ///
    /// The directory pass needs the facts of every record, so the records still count against
    /// `mem_budget` until emission starts. The cached directory paths are not counted.
    ///
    /// Paths are `root_prefix` joined with the on-disk names, so they match the paths the
    /// generic walker yields for the same root. Each hard-link name yields its own file item.
    /// Directories with a name-surrogate reparse point are yielded as symlink files and never descended.
    ///
    /// `to_component` converts one UTF-16 name to a path component (the caller uses the
    /// path codec on the wide name). A visitor error stops emission at that item and is
    /// returned unchanged.
    /// Checks the scan root before anything is visited. An empty success would let the
    /// rescan delete every row under the root.
    pub fn check_root(&self) -> Result<(), &'static str> {
        match self.records.get(&self.root) {
            None => Err("root record is not in the index (metafile range or unreadable)"),
            Some(f) if kind_of(f) != EntryKind::Directory => Err("root record is not a directory"),
            Some(_) => Ok(()),
        }
    }

    pub fn emit_items<E>(
        self,
        root_prefix: &RawPath,
        to_component: &dyn Fn(&[u16]) -> RawPath,
        visit: &mut dyn FnMut(WalkItem) -> Result<(), E>,
    ) -> Result<(), E> {
        let Self { root, records, .. } = self;
        // `check_root` makes these early returns unreachable in practice.
        let Some(root_facts) = records.get(&root) else {
            return Ok(());
        };
        if kind_of(root_facts) != EntryKind::Directory {
            return Ok(());
        }

        let dirs = reach_directories(&records, root, root_prefix, to_component);

        visit(WalkItem::Dir {
            path: root_prefix.clone(),
        })?;
        let mut others: Vec<&RawPath> = dirs
            .iter()
            .filter(|&(&record, _)| record != root)
            .map(|(_, node)| &node.path)
            .collect();
        others.sort_unstable();
        for path in others {
            visit(WalkItem::Dir { path: path.clone() })?;
        }

        // Consuming the map drops each record's facts as soon as its names are visited.
        for facts in records.into_values() {
            if kind_of(&facts) == EntryKind::Directory {
                continue;
            }
            for name in &facts.names {
                // Orphans, names under unreachable parents, and children of a directory at the
                // depth limit have no expanded parent here, so they are skipped.
                let Some(parent) = dirs.get(&name.parent).filter(|dir| dir.depth < MAX_DEPTH)
                else {
                    continue;
                };
                let Some(meta) = file_meta_for(&facts) else {
                    continue;
                };
                let path = parent
                    .path
                    .join_component(to_component(&name.name).as_bytes());
                visit(WalkItem::File { path, meta })?;
            }
        }
        Ok(())
    }

    /// Walk items under the root, collected and sorted by path bytes.
    ///
    /// A thin wrapper over [`Self::emit_items`] for callers that need the whole list. The sort
    /// lives here and not in `emit_items`: sorting every file would need every path in memory
    /// at once. A parent path is a strict prefix of its children's, so parents still come first.
    pub fn into_items(
        self,
        root_prefix: &RawPath,
        to_component: &dyn Fn(&[u16]) -> RawPath,
    ) -> Vec<WalkItem> {
        let mut items = Vec::new();
        let collected: Result<(), Infallible> =
            self.emit_items(root_prefix, to_component, &mut |item: WalkItem| {
                items.push(item);
                Ok(())
            });
        let Ok(()) = collected;
        items.sort_by(|a, b| path_of(a).cmp(path_of(b)));
        items
    }
}

/// Whether `record` is an NTFS metafile. The root directory (5) is not one.
fn is_system_metafile(record: u64) -> bool {
    record <= METAFILE_LAST_RECORD && record != ROOT_DIR_RECORD
}

fn record_cost(facts: &RecordFacts) -> usize {
    facts.names.iter().fold(RECORD_OVERHEAD_BYTES, |acc, name| {
        acc.saturating_add(name.name.len().saturating_mul(2))
            .saturating_add(size_of::<NameFact>())
    })
}

/// A directory the walk reached: its path, and how far it is from the walk root.
struct DirNode {
    path: RawPath,
    /// Only a directory shallower than [`MAX_DEPTH`] has its children walked.
    depth: usize,
}

/// Child directories of each directory, as `(record, name)` pairs.
///
/// Files are left out. The file phase reads them from their own records, so this index never
/// holds a file name. Records are visited in record order, so each list is deterministic.
fn index_dir_children(records: &BTreeMap<u64, RecordFacts>) -> HashMap<u64, Vec<(u64, &[u16])>> {
    let mut children: HashMap<u64, Vec<(u64, &[u16])>> = HashMap::new();
    for (&record, facts) in records {
        if kind_of(facts) != EntryKind::Directory {
            continue;
        }
        for name in &facts.names {
            children
                .entry(name.parent)
                .or_default()
                .push((record, name.name.as_slice()));
        }
    }
    children
}

/// Every directory under `root`, keyed by record number, found breadth-first.
///
/// The first name that reaches a directory fixes its path. The map doubles as the visited
/// set: it stops cycles and a directory reached through a second, corrupt name. The root is in
/// the map from the start, so NTFS's self-named "." edge is skipped without a separate check.
fn reach_directories(
    records: &BTreeMap<u64, RecordFacts>,
    root: u64,
    root_prefix: &RawPath,
    to_component: &dyn Fn(&[u16]) -> RawPath,
) -> HashMap<u64, DirNode> {
    let children = index_dir_children(records);
    let mut dirs = HashMap::from([(
        root,
        DirNode {
            path: root_prefix.clone(),
            depth: 0,
        },
    )]);
    let mut queue = VecDeque::from([(root, 0usize)]);

    while let Some((dir, depth)) = queue.pop_front() {
        let child_depth = depth.saturating_add(1);
        if child_depth > MAX_DEPTH {
            continue;
        }
        let Some(entries) = children.get(&dir) else {
            continue;
        };
        for &(child, name) in entries {
            if dirs.contains_key(&child) {
                continue;
            }
            let Some(parent) = dirs.get(&dir) else {
                continue;
            };
            let path = parent.path.join_component(to_component(name).as_bytes());
            dirs.insert(
                child,
                DirNode {
                    path,
                    depth: child_depth,
                },
            );
            queue.push_back((child, child_depth));
        }
    }
    dirs
}

fn path_of(item: &WalkItem) -> &RawPath {
    match item {
        WalkItem::Dir { path } | WalkItem::File { path, .. } | WalkItem::Error { path, .. } => path,
    }
}

/// Classifies a record as directory, symlink or regular file.
///
/// A record is a link only when its reparse tag is a name surrogate (symlink, junction,
/// mount point, LX symlink), the same rule std and walkdir apply. A cloud directory
/// (OneDrive) is therefore walked, like the walker does. If the tag could not be read,
/// a reparse directory is not descended (a conservative `Symlink`) and a file stays regular.
pub fn kind_of(f: &RecordFacts) -> EntryKind {
    let reparse = has_reparse_point(f);
    let surrogate = f
        .reparse_tag
        .is_some_and(|t| t & IO_REPARSE_TAG_NAME_SURROGATE != 0);
    let unknown_dir_reparse = f.is_dir && reparse && f.reparse_tag.is_none();
    match (f.is_dir, reparse && surrogate) {
        (_, true) => EntryKind::Symlink,
        (true, false) if unknown_dir_reparse => EntryKind::Symlink,
        (true, false) => EntryKind::Directory,
        (false, false) => EntryKind::RegularFile,
    }
}

fn has_reparse_point(f: &RecordFacts) -> bool {
    f.reparse_tag.is_some() || (f.attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

/// `attrs` bits from the Windows attributes, using the same mapping as `backend-generic`.
///
/// `HARDLINK` depends on the link count, so [`file_meta_of`] adds it.
pub fn attrs_of(f: &RecordFacts) -> i64 {
    let mut bits: i64 = 0;
    if (f.attrs & FILE_ATTRIBUTE_HIDDEN) != 0 {
        bits |= attrs::HIDDEN;
    }
    if (f.attrs & PLACEHOLDER_ATTRIBUTES) != 0 {
        bits |= attrs::PLACEHOLDER;
    }
    if kind_of(f) == EntryKind::Symlink {
        bits |= attrs::SYMLINK;
    }
    bits
}

/// Builds the `FileMeta` for one record with `links` hard links.
///
/// Returns `None` when the record number does not fit the 48-bit file reference or the
/// size does not fit `i64`. Both mean corrupt data, and the caller skips the record.
pub fn file_meta_of(f: &RecordFacts, links: i64) -> Option<FileMeta> {
    let file_id = file_reference(f)?.to_le_bytes().to_vec();
    let size = i64::try_from(f.size).ok()?;
    let mut attr_bits = attrs_of(f);
    if links > 1 {
        attr_bits |= attrs::HARDLINK;
    }
    Some(FileMeta {
        dev: None,
        file_id: Some(file_id),
        nlink: links,
        size,
        mtime_ns: mtime_ns(f.mtime_ft),
        ctime_ns: ctime_ns(f.ctime_ft),
        attrs: attr_bits,
        kind: kind_of(f),
    })
}

/// Link count is the number of non-DOS names on the volume, which matches NTFS hard links.
fn file_meta_for(f: &RecordFacts) -> Option<FileMeta> {
    let links = i64::try_from(f.names.len()).ok()?;
    file_meta_of(f, links)
}

/// NTFS file reference: record number in the low 48 bits, sequence in the high 16.
fn file_reference(f: &RecordFacts) -> Option<u64> {
    (f.record <= RECORD_NUMBER_MASK).then(|| f.record | (u64::from(f.sequence) << 48))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_PREFIX: &[u8] = b"C:\\scan";
    /// Cloud-files placeholder tag. The exact value is unconfirmed on real volumes.
    const CLOUD_TAG: u32 = 0x9000_601A;

    fn prefix() -> RawPath {
        RawPath::new(ROOT_PREFIX)
    }

    fn to_component(units: &[u16]) -> RawPath {
        RawPath::new(String::from_utf16_lossy(units).into_bytes())
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn name(parent: u64, text: &str, namespace: NameSpace) -> NameFact {
        NameFact {
            parent,
            name: units(text),
            namespace,
        }
    }

    fn win32(parent: u64, text: &str) -> NameFact {
        name(parent, text, NameSpace::Win32)
    }

    fn facts(record: u64, is_dir: bool, names: Vec<NameFact>) -> RecordFacts {
        RecordFacts {
            record,
            sequence: 1,
            in_use: true,
            is_dir,
            attrs: 0,
            reparse_tag: None,
            mtime_ft: 0,
            ctime_ft: 0,
            size: 0,
            names,
        }
    }

    fn dir(record: u64, names: Vec<NameFact>) -> RecordFacts {
        facts(record, true, names)
    }

    fn file(record: u64, names: Vec<NameFact>) -> RecordFacts {
        facts(record, false, names)
    }

    /// NTFS root directory: record 5, named "." under itself.
    fn root_dir() -> RecordFacts {
        dir(5, vec![name(5, ".", NameSpace::Win32)])
    }

    /// First record past the NTFS metafile range (0..=23). `push` drops those records, so
    /// fixtures that exercise the walk must number their directories from here on.
    const SCAN_ROOT: u64 = 24;

    /// Directory `record` that names itself ".", used as a walk root.
    fn root_at(record: u64) -> RecordFacts {
        dir(record, vec![name(record, ".", NameSpace::Win32)])
    }

    fn build(root: u64, records: Vec<RecordFacts>) -> VolumeTree {
        let mut tree = VolumeTree::new(root, 1 << 24);
        for record in records {
            tree.push(record).expect("test budget is large enough");
        }
        tree
    }

    fn items_of(root: u64, records: Vec<RecordFacts>) -> Vec<WalkItem> {
        build(root, records).into_items(&prefix(), &to_component)
    }

    fn paths(items: &[WalkItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| path_of(item).to_display())
            .collect()
    }

    fn file_entries(items: &[WalkItem]) -> Vec<(String, FileMeta)> {
        items
            .iter()
            .filter_map(|item| match item {
                WalkItem::File { path, meta } => Some((path.to_display(), meta.clone())),
                WalkItem::Dir { .. } | WalkItem::Error { .. } => None,
            })
            .collect()
    }

    #[test]
    fn into_items_should_build_nested_paths_when_tree_has_three_levels() {
        let items = items_of(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                dir(25, vec![win32(SCAN_ROOT, "Users")]),
                dir(26, vec![win32(25, "Bob")]),
                file(27, vec![win32(26, "a.txt")]),
            ],
        );
        assert_eq!(
            paths(&items),
            [
                "C:\\scan",
                "C:\\scan\\Users",
                "C:\\scan\\Users\\Bob",
                "C:\\scan\\Users\\Bob\\a.txt",
            ],
            "nested names must join under the root prefix, parents first"
        );
    }

    #[test]
    fn into_items_should_scope_to_subtree_when_root_is_not_volume_root() {
        let items = items_of(
            25,
            vec![
                root_at(SCAN_ROOT),
                dir(25, vec![win32(SCAN_ROOT, "scan")]),
                dir(26, vec![win32(25, "a")]),
                file(27, vec![win32(26, "f")]),
                dir(28, vec![win32(SCAN_ROOT, "other")]),
                file(29, vec![win32(28, "g")]),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan", "C:\\scan\\a", "C:\\scan\\a\\f"],
            "only records under the given root record may be yielded"
        );
    }

    #[test]
    fn into_items_should_yield_parents_first_when_records_arrive_children_first() {
        let items = items_of(
            SCAN_ROOT,
            vec![
                file(27, vec![win32(26, "c.txt")]),
                dir(26, vec![win32(25, "b")]),
                dir(25, vec![win32(SCAN_ROOT, "a")]),
                root_at(SCAN_ROOT),
            ],
        );
        assert_eq!(
            paths(&items),
            [
                "C:\\scan",
                "C:\\scan\\a",
                "C:\\scan\\a\\b",
                "C:\\scan\\a\\b\\c.txt"
            ],
            "a directory must precede its contents whatever the arrival order"
        );
    }

    #[test]
    fn into_items_should_yield_two_file_items_when_hard_link_has_two_names() {
        let items = items_of(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                file(30, vec![win32(SCAN_ROOT, "one.txt"), win32(25, "two.txt")]),
                dir(25, vec![win32(SCAN_ROOT, "Dir")]),
            ],
        );
        assert_eq!(
            file_entries(&items).len(),
            2,
            "each hard-link name must yield its own file item"
        );
    }

    #[test]
    fn into_items_should_yield_both_link_paths_when_hard_link_has_two_names() {
        let items = items_of(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                file(30, vec![win32(SCAN_ROOT, "one.txt"), win32(25, "two.txt")]),
                dir(25, vec![win32(SCAN_ROOT, "Dir")]),
            ],
        );
        let paths: Vec<String> = file_entries(&items)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            paths,
            ["C:\\scan\\Dir\\two.txt", "C:\\scan\\one.txt"],
            "each hard-link name must yield a file item at its own reachable path"
        );
    }

    #[test]
    fn into_items_should_set_nlink_two_and_hardlink_bit_when_hard_link_has_two_names() {
        let items = items_of(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                file(30, vec![win32(SCAN_ROOT, "one.txt"), win32(25, "two.txt")]),
                dir(25, vec![win32(SCAN_ROOT, "Dir")]),
            ],
        );
        let flags: Vec<(i64, bool)> = file_entries(&items)
            .iter()
            .map(|(_, meta)| (meta.nlink, attrs::has(meta.attrs, attrs::HARDLINK)))
            .collect();
        assert_eq!(
            flags,
            [(2, true), (2, true)],
            "every name of a two-link file must report nlink 2 and the HARDLINK bit"
        );
    }

    #[test]
    fn into_items_should_skip_name_when_only_name_is_dos() {
        let items = items_of(
            5,
            vec![
                root_dir(),
                file(30, vec![name(5, "FOO~1.TXT", NameSpace::Dos)]),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan"],
            "a DOS-only 8.3 name must not produce an item"
        );
    }

    #[test]
    fn into_items_should_use_win32_name_when_record_has_dos_and_win32_names() {
        let items = items_of(
            5,
            vec![
                root_dir(),
                file(
                    30,
                    vec![name(5, "FOO~1.TXT", NameSpace::Dos), win32(5, "foo.txt")],
                ),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan", "C:\\scan\\foo.txt"],
            "the Win32 name must be used and the DOS spelling ignored"
        );
    }

    #[test]
    fn into_items_should_use_win32_and_dos_name_once_when_namespace_is_combined() {
        let items = items_of(
            5,
            vec![
                root_dir(),
                file(30, vec![name(5, "FOO.TXT", NameSpace::Win32AndDos)]),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan", "C:\\scan\\FOO.TXT"],
            "a Win32AndDos name must yield exactly one item"
        );
    }

    #[test]
    fn into_items_should_skip_record_when_not_in_use() {
        let unused = RecordFacts {
            in_use: false,
            ..file(30, vec![win32(5, "x")])
        };
        let items = items_of(5, vec![root_dir(), unused]);
        assert_eq!(
            paths(&items),
            ["C:\\scan"],
            "a record that is not in use must be skipped"
        );
    }

    #[test]
    fn into_items_should_skip_metafile_subtree_when_parent_is_extend() {
        let items = items_of(
            5,
            vec![
                root_dir(),
                dir(11, vec![win32(5, "$Extend")]),
                file(30, vec![win32(11, "usn")]),
                file(0, vec![win32(5, "$MFT")]),
                file(40, vec![win32(5, "ok.txt")]),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan", "C:\\scan\\ok.txt"],
            "metafiles and anything under them must be skipped"
        );
    }

    #[test]
    fn into_items_should_exclude_orphan_when_parent_record_is_missing() {
        let items = items_of(5, vec![root_dir(), file(30, vec![win32(99, "lost.txt")])]);
        assert_eq!(
            paths(&items),
            ["C:\\scan"],
            "a record whose parent chain does not reach the root must be excluded"
        );
    }

    #[test]
    fn into_items_should_terminate_when_directory_names_itself_as_parent() {
        let items = items_of(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                dir(25, vec![win32(25, "a"), win32(SCAN_ROOT, "a")]),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan", "C:\\scan\\a"],
            "a self-referencing directory must be walked once and the walk must end"
        );
    }

    #[test]
    fn into_items_should_stop_descending_when_depth_limit_is_reached() {
        // Chain 24 -> 25 -> 26 -> ... of single-letter directories, longer than the limit.
        let chain_end = SCAN_ROOT + MAX_DEPTH as u64 + 10;
        let mut records = vec![root_at(SCAN_ROOT)];
        records.extend(
            ((SCAN_ROOT + 1)..=chain_end).map(|record| dir(record, vec![win32(record - 1, "d")])),
        );
        let items = items_of(SCAN_ROOT, records);
        assert_eq!(
            items.len(),
            MAX_DEPTH + 1,
            "the root plus MAX_DEPTH directories must be yielded, and no deeper ones"
        );
    }

    #[test]
    fn into_items_should_exclude_children_when_directory_has_reparse_point() {
        let link = RecordFacts {
            reparse_tag: Some(IO_REPARSE_TAG_MOUNT_POINT),
            ..dir(25, vec![win32(SCAN_ROOT, "link")])
        };
        let items = items_of(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                link,
                file(26, vec![win32(25, "inside.txt")]),
            ],
        );
        assert_eq!(
            paths(&items),
            ["C:\\scan", "C:\\scan\\link"],
            "a reparse directory must be yielded as a link and its contents excluded"
        );
    }

    #[test]
    fn into_items_should_yield_symlink_file_when_directory_has_reparse_point() {
        let link = RecordFacts {
            reparse_tag: Some(IO_REPARSE_TAG_MOUNT_POINT),
            ..dir(25, vec![win32(SCAN_ROOT, "link")])
        };
        let items = items_of(SCAN_ROOT, vec![root_at(SCAN_ROOT), link]);
        let kinds: Vec<(String, EntryKind)> = file_entries(&items)
            .into_iter()
            .map(|(path, meta)| (path, meta.kind))
            .collect();
        assert_eq!(
            kinds,
            [("C:\\scan\\link".to_string(), EntryKind::Symlink)],
            "the reparse directory must be a File item of kind Symlink"
        );
    }

    #[test]
    fn kind_of_should_return_regular_file_when_reparse_tag_is_cloud() {
        let placeholder = RecordFacts {
            reparse_tag: Some(CLOUD_TAG),
            ..file(30, vec![win32(5, "cloud.bin")])
        };
        assert_eq!(
            kind_of(&placeholder),
            EntryKind::RegularFile,
            "a cloud-tagged file must stay a regular file"
        );
    }

    #[test]
    fn file_meta_of_should_set_placeholder_bit_when_offline_attribute_is_set() {
        let placeholder = RecordFacts {
            attrs: FILE_ATTRIBUTE_OFFLINE,
            reparse_tag: Some(CLOUD_TAG),
            ..file(30, vec![win32(5, "cloud.bin")])
        };
        let meta = file_meta_of(&placeholder, 1).expect("valid record");
        assert!(
            attrs::has(meta.attrs, attrs::PLACEHOLDER),
            "OFFLINE attribute must map to the PLACEHOLDER bit, got {:#x}",
            meta.attrs
        );
    }

    #[test]
    fn kind_of_should_return_symlink_when_file_reparse_tag_is_symlink() {
        let link = RecordFacts {
            reparse_tag: Some(IO_REPARSE_TAG_SYMLINK),
            ..file(30, vec![win32(5, "link.txt")])
        };
        assert_eq!(
            kind_of(&link),
            EntryKind::Symlink,
            "a file with the symlink reparse tag must be a Symlink"
        );
    }

    #[test]
    fn kind_of_should_return_directory_when_no_reparse_point() {
        assert_eq!(
            kind_of(&root_dir()),
            EntryKind::Directory,
            "a plain directory must be a Directory"
        );
    }

    #[test]
    fn attrs_of_should_set_hidden_bit_when_hidden_attribute_is_set() {
        let hidden = RecordFacts {
            attrs: FILE_ATTRIBUTE_HIDDEN,
            ..file(30, vec![win32(5, "h.txt")])
        };
        assert!(
            attrs::has(attrs_of(&hidden), attrs::HIDDEN),
            "FILE_ATTRIBUTE_HIDDEN must map to the HIDDEN bit"
        );
    }

    #[test]
    fn file_meta_of_should_pack_record_and_sequence_into_file_id() {
        let record = RecordFacts {
            sequence: 5,
            ..file(0x1234, vec![win32(5, "x")])
        };
        let meta = file_meta_of(&record, 1).expect("valid record");
        assert_eq!(
            meta.file_id,
            Some((0x1234u64 | (5u64 << 48)).to_le_bytes().to_vec()),
            "file id must be the little-endian reference with the sequence in bits 48..64"
        );
    }

    #[test]
    fn file_meta_of_should_return_none_when_record_exceeds_48_bits() {
        let wide = file(1u64 << 48, vec![win32(5, "x")]);
        assert_eq!(
            file_meta_of(&wide, 1),
            None,
            "a record number above 48 bits cannot form a file reference"
        );
    }

    #[test]
    fn file_meta_of_should_return_none_when_size_exceeds_i64() {
        let huge = RecordFacts {
            size: u64::MAX,
            ..file(30, vec![win32(5, "big")])
        };
        assert_eq!(
            file_meta_of(&huge, 1),
            None,
            "a size above i64::MAX must be rejected, not wrapped"
        );
    }

    #[test]
    fn file_meta_of_should_not_set_hardlink_bit_when_single_link() {
        let meta = file_meta_of(&file(30, vec![win32(5, "x")]), 1).expect("valid record");
        assert!(
            !attrs::has(meta.attrs, attrs::HARDLINK),
            "one link must not set the HARDLINK bit"
        );
    }

    #[test]
    fn file_meta_of_should_set_mtime_zero_when_mtime_is_before_1970() {
        let old = RecordFacts {
            mtime_ft: 1,
            ..file(30, vec![win32(5, "old")])
        };
        let meta = file_meta_of(&old, 1).expect("valid record");
        assert_eq!(meta.mtime_ns, 0, "a pre-1970 mtime must fall back to 0");
    }

    #[test]
    fn file_meta_of_should_set_ctime_none_when_ctime_is_before_1970() {
        let old = RecordFacts {
            ctime_ft: 1,
            ..file(30, vec![win32(5, "old")])
        };
        let meta = file_meta_of(&old, 1).expect("valid record");
        assert_eq!(meta.ctime_ns, None, "a pre-1970 ctime must be None");
    }

    #[test]
    fn push_should_return_budget_error_when_estimate_exceeds_budget() {
        let mut tree = VolumeTree::new(5, 16);
        let result = tree.push(root_dir());
        assert!(
            matches!(result, Err(BudgetExceeded { limit_bytes: 16 })),
            "a record over a 16 byte budget must fail with BudgetExceeded, got {result:?}"
        );
    }

    fn nested_records() -> Vec<RecordFacts> {
        vec![
            root_at(SCAN_ROOT),
            dir(25, vec![win32(SCAN_ROOT, "Users")]),
            dir(26, vec![win32(25, "Bob")]),
            file(27, vec![win32(26, "a.txt")]),
            file(30, vec![win32(SCAN_ROOT, "one.txt"), win32(25, "two.txt")]),
        ]
    }

    #[test]
    fn emit_items_should_yield_same_items_as_into_items() {
        let mut emitted = Vec::new();
        let collected: Result<(), Infallible> = build(SCAN_ROOT, nested_records()).emit_items(
            &prefix(),
            &to_component,
            &mut |item: WalkItem| {
                emitted.push(item);
                Ok(())
            },
        );
        let Ok(()) = collected;
        emitted.sort_by(|a, b| path_of(a).cmp(path_of(b)));
        let expected = build(SCAN_ROOT, nested_records()).into_items(&prefix(), &to_component);
        assert_eq!(
            (paths(&emitted), file_entries(&emitted)),
            (paths(&expected), file_entries(&expected)),
            "emit_items must yield the same items as into_items once both are in path order"
        );
    }

    #[test]
    fn emit_items_should_stop_and_return_error_when_visit_fails() {
        let tree = build(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                dir(25, vec![win32(SCAN_ROOT, "Users")]),
                file(26, vec![win32(25, "a.txt")]),
            ],
        );
        let mut calls = 0usize;
        let result = tree.emit_items(&prefix(), &to_component, &mut |_item: WalkItem| {
            calls += 1;
            if calls == 2 {
                Err("stop")
            } else {
                Ok(())
            }
        });
        assert_eq!(
            result,
            Err("stop"),
            "the visitor's error must be returned unchanged"
        );
        assert_eq!(
            calls, 2,
            "emission must stop at the failing item, got {calls} visits"
        );
    }

    #[test]
    fn emit_items_should_emit_all_directories_before_any_file() {
        // "A.txt" sorts before "b" by path bytes, so a path-sorted walk would put the file first.
        let tree = build(
            SCAN_ROOT,
            vec![
                root_at(SCAN_ROOT),
                dir(25, vec![win32(SCAN_ROOT, "b")]),
                file(26, vec![win32(SCAN_ROOT, "A.txt")]),
            ],
        );
        let mut kinds: Vec<&'static str> = Vec::new();
        let collected: Result<(), Infallible> =
            tree.emit_items(&prefix(), &to_component, &mut |item: WalkItem| {
                kinds.push(match item {
                    WalkItem::Dir { .. } => "dir",
                    WalkItem::File { .. } => "file",
                    WalkItem::Error { .. } => "error",
                });
                Ok(())
            });
        let Ok(()) = collected;
        assert_eq!(
            kinds,
            ["dir", "dir", "file"],
            "the root and the subdirectory must both precede the file"
        );
    }

    #[test]
    fn remove_should_drop_record_and_release_budget() {
        let mut tree = build(5, vec![root_dir()]);
        let before = tree.used_bytes;
        tree.push(file(30, vec![win32(5, "x")]))
            .expect("test budget is large enough");
        tree.remove(30);
        assert_eq!(
            tree.used_bytes, before,
            "removing a record must release exactly the cost it added"
        );
        assert!(
            tree.records_iter().all(|facts| facts.record != 30),
            "a removed record must no longer be stored"
        );
    }

    #[test]
    fn remove_should_do_nothing_when_record_absent() {
        let mut tree = build(5, vec![root_dir()]);
        let before = tree.used_bytes;
        tree.remove(99);
        assert_eq!(
            tree.used_bytes, before,
            "removing an absent record must not change the budget"
        );
        assert_eq!(
            tree.records_iter().count(),
            1,
            "removing an absent record must not drop stored records"
        );
    }

    #[test]
    fn records_iter_should_yield_in_record_order() {
        let tree = build(
            SCAN_ROOT,
            vec![
                file(40, vec![win32(SCAN_ROOT, "c")]),
                dir(25, vec![win32(SCAN_ROOT, "b")]),
                root_at(SCAN_ROOT),
            ],
        );
        let order: Vec<u64> = tree.records_iter().map(|facts| facts.record).collect();
        assert_eq!(
            order,
            [SCAN_ROOT, 25, 40],
            "records must be yielded in ascending record-number order"
        );
    }

    #[test]
    fn from_facts_should_match_push_when_same_records() {
        let built = VolumeTree::from_facts(SCAN_ROOT, 1 << 24, nested_records())
            .expect("test budget is large enough");
        let pushed = build(SCAN_ROOT, nested_records());
        assert_eq!(
            built.records_iter().collect::<Vec<_>>(),
            pushed.records_iter().collect::<Vec<_>>(),
            "from_facts must store the same records as push, in the same order"
        );
    }

    #[test]
    fn from_facts_should_error_when_budget_too_small() {
        let result = VolumeTree::from_facts(5, 16, vec![root_dir()]);
        assert!(
            matches!(result, Err(BudgetExceeded { limit_bytes: 16 })),
            "a budget too small for one record must fail with BudgetExceeded"
        );
    }
}
