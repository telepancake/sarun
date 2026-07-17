//! The single catalog of reserved per-box filesystem nodes.
//!
//! These names are transport-independent and never become depot rows.  Keeping
//! their identity and attributes together prevents FUSE, virtio-fs, lookup,
//! open, and readdir paths from inventing separate special-name rules.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use crate::capture::BoxState;
use crate::sarunfs::{NodeAttr, NodeKey, NodeKind};

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

/// Runtime state for nodes that are presented by the engine rather than by a
/// captured or backing layer.  Filesystem policy asks this service about
/// projections and opens synthetic handles; neither FUSE nor virtio-fs owns
/// their lifetime, output routing, or jobserver behavior.
pub(crate) struct SyntheticRuntime {
    projections: RwLock<HashMap<NodeKey, PathBuf>>,
    echo: RwLock<HashMap<i64, Arc<Mutex<UnixStream>>>>,
    sink_open: Mutex<HashMap<i64, u32>>,
    muted: RwLock<HashMap<i32, i64>>,
    guest_jobserver: Mutex<GuestJobserver>,
}

struct GuestJobserver {
    // Guest pid namespaces overlap each other and the host. Negative synthetic
    // actor ids keep the engine-global slip ledger collision-free without ever
    // passing a guest pid to host pidfd_open; remove_box reaps these actors.
    next_actor: i32,
    actors: HashMap<(i64, u32), i32>,
}

impl SyntheticRuntime {
    pub(crate) fn new() -> Self {
        Self {
            projections: RwLock::new(HashMap::new()),
            echo: RwLock::new(HashMap::new()),
            sink_open: Mutex::new(HashMap::new()),
            muted: RwLock::new(HashMap::new()),
            guest_jobserver: Mutex::new(GuestJobserver {
                next_actor: -1,
                actors: HashMap::new(),
            }),
        }
    }

    pub(crate) fn project(&self, box_id: i64, rel: &str, source: PathBuf) {
        self.projections
            .write()
            .unwrap()
            .insert((box_id, rel.to_owned()), source);
    }

    pub(crate) fn projected(&self, box_id: i64, rel: &str) -> Option<PathBuf> {
        self.projections
            .read()
            .unwrap()
            .get(&(box_id, rel.to_owned()))
            .cloned()
    }

    pub(crate) fn projected_children(&self, box_id: i64, parent: &str) -> Vec<String> {
        self.projections
            .read()
            .unwrap()
            .iter()
            .filter_map(|((owner, projected), _)| {
                if *owner != box_id {
                    return None;
                }
                let (projected_parent, name) = projected
                    .rsplit_once('/')
                    .unwrap_or(("", projected.as_str()));
                (projected_parent == parent).then(|| name.to_owned())
            })
            .collect()
    }

    pub(crate) fn remove_box(&self, box_id: i64) {
        self.projections
            .write()
            .unwrap()
            .retain(|(owner, _), _| *owner != box_id);
        self.clear_echo(box_id);
        let actors = {
            let mut guest = self.guest_jobserver.lock().unwrap();
            let actors = guest
                .actors
                .iter()
                .filter_map(|(&(owner, _), &actor)| (owner == box_id).then_some(actor))
                .collect::<Vec<_>>();
            guest.actors.retain(|&(owner, _), _| owner != box_id);
            actors
        };
        if !actors.is_empty() {
            let _ = crate::slippool::global().lock().unwrap().reap_box(&actors);
        }
    }

    pub(crate) fn child_ids(&self, boxes: &BTreeMap<i64, Arc<BoxState>>, parent: i64) -> Vec<i64> {
        boxes
            .values()
            .filter(|box_state| box_state.parent() == Some(parent))
            .map(|box_state| box_state.id)
            .collect()
    }

    pub(crate) fn is_child(
        &self,
        boxes: &BTreeMap<i64, Arc<BoxState>>,
        parent: i64,
        child: i64,
    ) -> bool {
        boxes.get(&child).and_then(|box_state| box_state.parent()) == Some(parent)
    }

    pub(crate) fn set_echo(&self, box_id: i64, conn: Arc<Mutex<UnixStream>>) {
        self.echo.write().unwrap().insert(box_id, conn);
    }

    pub(crate) fn clear_echo(&self, box_id: i64) {
        self.echo.write().unwrap().remove(&box_id);
        self.sink_open.lock().unwrap().remove(&box_id);
    }

    pub(crate) fn echo_writer(&self, box_id: i64) -> Option<Arc<Mutex<UnixStream>>> {
        self.echo.read().unwrap().get(&box_id).cloned()
    }

    pub(crate) fn mute_add(&self, host_tgid: i32, box_id: i64) {
        if host_tgid > 0 {
            self.muted.write().unwrap().insert(host_tgid, box_id);
        }
    }

    pub(crate) fn mute_remove(&self, host_tgid: i32) {
        self.muted.write().unwrap().remove(&host_tgid);
    }

    fn muted_owner(&self, host_tgid: i32) -> Option<i64> {
        self.muted.read().unwrap().get(&host_tgid).copied()
    }

    fn echo_send(&self, box_id: i64, stream: i32, data: &[u8]) {
        let Some(conn) = self.echo_writer(box_id) else {
            return;
        };
        let frame = crate::frames::encode(
            crate::frames::FRAME_ECHO,
            &crate::frames::echo_payload(stream as u8, data),
        );
        let _ = conn.lock().unwrap().write_all(&frame);
    }

    pub(crate) fn sink_opened(&self, box_id: i64) {
        *self.sink_open.lock().unwrap().entry(box_id).or_insert(0) += 1;
    }

    pub(crate) fn sink_released(&self, box_id: i64) {
        let zero = {
            let mut open = self.sink_open.lock().unwrap();
            if let Some(count) = open.get_mut(&box_id) {
                *count = count.saturating_sub(1);
                *count == 0
            } else {
                false
            }
        };
        if zero {
            if let Some(conn) = self.echo_writer(box_id) {
                let frame = crate::frames::encode(crate::frames::FRAME_ECHO_DONE, &[]);
                let _ = conn.lock().unwrap().write_all(&frame);
            }
        }
    }

    pub(crate) fn write_sink(
        &self,
        host_pid: u32,
        host_tgid: i32,
        box_state: Option<&BoxState>,
        box_id: i64,
        stream: i32,
        data: &[u8],
    ) {
        let record = self
            .muted_owner(host_tgid)
            .is_none_or(|owner| owner == box_id);
        if record {
            if let Some(box_state) = box_state {
                box_state.add_output(stream, host_pid, data);
            }
        }
        self.echo_send(box_id, stream, data);
    }

    fn acquire_jobserver(
        &self,
        actor: i32,
        reply: Box<dyn crate::slippool::SlipReply>,
        nonblocking: bool,
        watch_host_pid: bool,
    ) {
        let watch = crate::slippool::global()
            .lock()
            .unwrap()
            .acquire(actor, reply, nonblocking);
        if watch_host_pid {
            if let crate::slippool::Watch::Pid(pid) = watch {
                crate::slippool::watch_pid(pid);
            }
        }
    }

    pub(crate) fn acquire_host_jobserver(
        &self,
        host_tgid: i32,
        reply: Box<dyn crate::slippool::SlipReply>,
        nonblocking: bool,
    ) {
        self.acquire_jobserver(host_tgid, reply, nonblocking, true);
    }

    fn guest_actor(&self, box_id: i64, guest_pid: u32) -> i32 {
        let mut guest = self.guest_jobserver.lock().unwrap();
        if let Some(actor) = guest.actors.get(&(box_id, guest_pid)) {
            return *actor;
        }
        let actor = guest.next_actor;
        guest.next_actor = guest.next_actor.checked_sub(1).unwrap_or(-1);
        guest.actors.insert((box_id, guest_pid), actor);
        actor
    }

    pub(crate) fn acquire_guest_jobserver_blocking(
        &self,
        box_id: i64,
        guest_pid: u32,
        nonblocking: bool,
    ) -> std::io::Result<u8> {
        struct Reply(std::sync::mpsc::Sender<bool>);
        impl crate::slippool::SlipReply for Reply {
            fn grant(self: Box<Self>) {
                let _ = self.0.send(true);
            }

            fn deny_again(self: Box<Self>) {
                let _ = self.0.send(false);
            }
        }

        let (send, receive) = std::sync::mpsc::channel();
        let actor = self.guest_actor(box_id, guest_pid);
        self.acquire_jobserver(actor, Box::new(Reply(send)), nonblocking, false);
        match receive.recv() {
            Ok(true) => Ok(crate::slippool::SLIP),
            Ok(false) => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "jobserver reply channel closed",
            )),
        }
    }

    pub(crate) fn release_host_jobserver(&self, host_tgid: i32) {
        let _ = crate::slippool::global().lock().unwrap().release(host_tgid);
    }

    pub(crate) fn release_guest_jobserver(&self, box_id: i64, guest_pid: u32) {
        let actor = self.guest_actor(box_id, guest_pid);
        let _ = crate::slippool::global().lock().unwrap().release(actor);
    }

    #[cfg(test)]
    pub(crate) fn projection_count(&self) -> usize {
        self.projections.read().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

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

    #[test]
    fn runtime_owns_projection_and_sink_lifetimes() {
        let runtime = SyntheticRuntime::new();
        runtime.project(7, "init", PathBuf::from("/engine/init"));
        runtime.project(7, "etc/resolv.conf", PathBuf::from("/engine/resolv"));
        runtime.project(8, "init", PathBuf::from("/engine/other-init"));
        assert_eq!(
            runtime.projected(7, "init"),
            Some(PathBuf::from("/engine/init"))
        );
        assert_eq!(runtime.projected_children(7, ""), vec!["init"]);
        assert_eq!(runtime.projected_children(7, "etc"), vec!["resolv.conf"]);

        let (writer, mut reader) = UnixStream::pair().unwrap();
        runtime.set_echo(7, Arc::new(Mutex::new(writer)));
        runtime.sink_opened(7);
        runtime.sink_opened(7);
        runtime.write_sink(123, 123, None, 7, 1, b"hello");
        runtime.sink_released(7);
        runtime.sink_released(7);

        let mut expected = crate::frames::encode(
            crate::frames::FRAME_ECHO,
            &crate::frames::echo_payload(1, b"hello"),
        );
        expected.extend(crate::frames::encode(crate::frames::FRAME_ECHO_DONE, &[]));
        let mut actual = vec![0; expected.len()];
        reader.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected);

        runtime.remove_box(7);
        assert_eq!(runtime.projection_count(), 1);
        assert!(runtime.echo_writer(7).is_none());
    }
}
