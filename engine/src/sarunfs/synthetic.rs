//! The single catalog of reserved per-box filesystem nodes.
//!
//! These names are transport-independent and never become depot rows.  Keeping
//! their identity and attributes together prevents FUSE, virtio-fs, lookup,
//! open, and readdir paths from inventing separate special-name rules.

use crate::sarunfs::{NodeAttr, NodeKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyntheticNode {
    Stdout,
    Stderr,
    Children,
    Jobserver,
}

impl SyntheticNode {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Stdout => ".slopbox-stdout",
            Self::Stderr => ".slopbox-stderr",
            Self::Children => ".slopbox-kids",
            Self::Jobserver => ".slopbox-jobserver",
        }
    }

    pub(crate) fn at(rel: &str) -> Option<Self> {
        [Self::Stdout, Self::Stderr, Self::Children, Self::Jobserver]
            .into_iter()
            .find(|node| node.name() == rel)
    }

    pub(crate) const fn stream(self) -> Option<i32> {
        match self {
            Self::Stdout => Some(0),
            Self::Stderr => Some(1),
            _ => None,
        }
    }

    pub(crate) const fn is_file(self) -> bool {
        !matches!(self, Self::Children)
    }

    pub(crate) fn attr(self, inode: u64) -> NodeAttr {
        let directory = matches!(self, Self::Children);
        NodeAttr {
            inode,
            size: 0,
            blocks: 0,
            atime: std::time::UNIX_EPOCH,
            mtime: std::time::UNIX_EPOCH,
            ctime: std::time::UNIX_EPOCH,
            crtime: std::time::UNIX_EPOCH,
            kind: if directory {
                NodeKind::Directory
            } else {
                NodeKind::RegularFile
            },
            perm: if directory { 0o755 } else { 0o666 },
            nlink: if directory { 2 } else { 1 },
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_nodes_have_one_name_kind_and_attribute_definition() {
        let nodes = [
            SyntheticNode::Stdout,
            SyntheticNode::Stderr,
            SyntheticNode::Children,
            SyntheticNode::Jobserver,
        ];
        let mut names = std::collections::BTreeSet::new();
        for node in nodes {
            assert!(names.insert(node.name()));
            assert_eq!(SyntheticNode::at(node.name()), Some(node));
            assert_eq!(node.attr(42).kind == NodeKind::RegularFile, node.is_file());
        }
        assert_eq!(SyntheticNode::Stdout.stream(), Some(0));
        assert_eq!(SyntheticNode::Stderr.stream(), Some(1));
        assert_eq!(SyntheticNode::Jobserver.stream(), None);
        assert_eq!(SyntheticNode::at("ordinary"), None);
    }
}
