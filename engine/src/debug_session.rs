//! Descriptor-only resources for one QEMU debug session.
//!
//! Debug artifacts and byte streams cross the authenticated registration
//! channel as owned descriptors.  This module intentionally has no pathname,
//! environment, socket-directory, or child-command representation.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

/// Resources selected from named Sarun boxes and delivered by the engine.
pub(crate) struct DebugSessionResources {
    boot_kernel: OwnedFd,
    qemu_rsp: UnixStream,
    image: Option<DebugGuestImage>,
}

/// One engine-validated opaque initramfs launch.  The enum selects the whole
/// QEMU machine contract; callers cannot append arbitrary device arguments.
pub(crate) struct DebugGuestImage {
    initramfs: OwnedFd,
    profile: crate::generated_wire::DebugImageProfile,
    init: String,
}

impl DebugSessionResources {
    pub(crate) fn from_registration(
        boot_kernel: OwnedFd,
        qemu_rsp: UnixStream,
        image: Option<DebugGuestImage>,
    ) -> Self {
        Self {
            boot_kernel,
            qemu_rsp,
            image,
        }
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, UnixStream, Option<DebugGuestImage>) {
        (self.boot_kernel, self.qemu_rsp, self.image)
    }

    pub(crate) fn has_guest_image(&self) -> bool {
        self.image.is_some()
    }
}

impl DebugGuestImage {
    pub(crate) fn from_registration(
        initramfs: OwnedFd,
        profile: crate::generated_wire::DebugImageProfile,
        init: String,
    ) -> Self {
        Self {
            initramfs,
            profile,
            init,
        }
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, crate::generated_wire::DebugImageProfile, String) {
        (self.initramfs, self.profile, self.init)
    }
}
