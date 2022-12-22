//! Implementation of software buffering for X11.
//!
//! This module converts the input buffer into an XImage and then sends it over the wire to be
//! drawn. A more effective implementation would use shared memory instead of the wire. In
//! addition, we may also want to blit to a pixmap instead of a window.

use crate::SwBufError;
use nix::libc::{shmget, shmat, IPC_PRIVATE, shmctl, shmdt, IPC_RMID};
use raw_window_handle::{XlibDisplayHandle, XlibWindowHandle};

use std::io;
use std::mem;
use std::os::raw::{c_char, c_uint};
use std::ptr::{null_mut, NonNull};

use x11_dl::xlib::{Display, Visual, Xlib, ZPixmap, GC};
use x11_dl::xshm::{Xext as XShm, XShmSegmentInfo};

/// The handle to an X11 drawing context.
pub struct X11Impl {
    /// The window handle.
    window_handle: XlibWindowHandle,

    /// The display handle.
    display_handle: XlibDisplayHandle,

    /// Reference to the X11 shared library.
    xlib: Xlib,

    /// Reference to the X11 shared memory library.
    xshm: Option<ShmExtension>,

    /// The graphics context for drawing.
    gc: GC,

    /// Information about the screen to use for drawing.
    visual: *mut Visual,

    /// The depth (bits per pixel) of the drawing context.
    depth: i32,
}

/// SHM-specific information.
struct ShmExtension {
    /// The shared memory library.
    xshm: XShm,

    /// Pointer to the shared memory segment, as well as its current size.
    shmseg: Option<ShmSegment>,
}

/// An SHM segment.
struct ShmSegment {
    /// The shared memory segment ID.
    id: i32,

    /// The shared memory segment pointer.
    ptr: NonNull<i8>,

    /// The size of the shared memory segment.
    size: usize,
}

impl X11Impl {
    /// Create a new `X11Impl` from a `XlibWindowHandle` and `XlibDisplayHandle`.
    ///
    /// # Safety
    ///
    /// The `XlibWindowHandle` and `XlibDisplayHandle` must be valid.
    pub unsafe fn new(
        window_handle: XlibWindowHandle,
        display_handle: XlibDisplayHandle,
    ) -> Result<Self, SwBufError> {
        // Try to open the X11 shared library.
        let lib = match Xlib::open() {
            Ok(lib) => lib,
            Err(e) => {
                return Err(SwBufError::PlatformError(
                    Some("Failed to open Xlib".into()),
                    Some(Box::new(e)),
                ))
            }
        };

        // Validate the handles to ensure that they aren't incomplete.
        if display_handle.display.is_null() {
            return Err(SwBufError::IncompleteDisplayHandle);
        }

        if window_handle.window == 0 {
            return Err(SwBufError::IncompleteWindowHandle);
        }

        // Get the screen number from the handle.
        // NOTE: By default, XlibDisplayHandle sets the screen number to 0. If we see a zero,
        // it could mean either screen index zero, or that the screen number was not set. We
        // can't tell which, so we'll just assume that the screen number was not set.
        let screen = match display_handle.screen {
            0 => (lib.XDefaultScreen)(display_handle.display as *mut Display),
            screen => screen,
        };

        // Use the default graphics context, visual and depth for this screen.
        let gc = (lib.XDefaultGC)(display_handle.display as *mut Display, screen);
        let visual = (lib.XDefaultVisual)(display_handle.display as *mut Display, screen);
        let depth = (lib.XDefaultDepth)(display_handle.display as *mut Display, screen);

        // See if we can load the XShm extension.
        let xshm = XShm::open()
            .ok()
            .filter(|shm| (shm.XShmQueryExtension)(display_handle.display as *mut Display) != 0);

        Ok(Self {
            window_handle,
            display_handle,
            xlib: lib,
            xshm: xshm.map(|xshm| ShmExtension { xshm, shmseg: None }),
            gc,
            visual,
            depth,
        })
    }

    pub(crate) unsafe fn set_buffer(&mut self, buffer: &[u32], width: u16, height: u16) {
        if self.shm_set(buffer, width, height).is_err() {
            self.fallback_set(buffer, width, height);
        }
    }

    /// Set the buffer to the given image using shared memory.
    unsafe fn shm_set(&mut self, buffer: &[u32], width: u16, height: u16) -> io::Result<()> {
        let shm_ext = match self.xshm.as_mut() {
            Some(shm_ext) => shm_ext,
            None => return Err(io::Error::new(io::ErrorKind::Other, "XShm not available")),
        };

        // Get the size of the shared memory segment.
        let shmseg_size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|size| size.checked_mul(4))
            .expect("Buffer size overflow");

        // Create the shared memory segment if it doesn't exist, or if it's the wrong size.
        let shmseg = match &mut shm_ext.shmseg {
            None => shm_ext.shmseg.insert(ShmSegment::new(shmseg_size)?),
            Some(ref shmseg) if shmseg.size < shmseg_size => {
                shm_ext.shmseg.insert(ShmSegment::new(shmseg_size)?)
            }
            Some(shmseg) => shmseg,
        };

        // Create the basic image.
        let mut seg: XShmSegmentInfo = mem::zeroed();
        let image = (shm_ext.xshm.XShmCreateImage)(
            self.display_handle.display as *mut Display,
            self.visual,
            self.depth as u32,
            ZPixmap,
            shmseg.ptr.as_ptr(),
            &mut seg,
            width as u32,
            height as u32,
        );

        Ok(())
    }

    /// Fall back to using `XPutImage` to draw the buffer.
    unsafe fn fallback_set(&mut self, buffer: &[u32], width: u16, height: u16) {
        // Create the image from the buffer.
        let image = (self.xlib.XCreateImage)(
            self.display_handle.display as *mut Display,
            self.visual,
            self.depth as u32,
            ZPixmap,
            0,
            (buffer.as_ptr()) as *mut c_char,
            width as u32,
            height as u32,
            32,
            (width * 4) as i32,
        );

        // Draw the image to the window.
        (self.xlib.XPutImage)(
            self.display_handle.display as *mut Display,
            self.window_handle.window,
            self.gc,
            image,
            0,
            0,
            0,
            0,
            width as c_uint,
            height as c_uint,
        );

        // Delete the image data.
        (*image).data = null_mut();
        (self.xlib.XDestroyImage)(image);
    }
}

impl ShmSegment {
    /// Create a new `ShmSegment` with the given size.
    fn new(size: usize) -> io::Result<Self> {
        unsafe {
            // Create the shared memory segment.
            let id = shmget(IPC_PRIVATE, size, 0o600);
            if id == -1 {
                return Err(io::Error::last_os_error());
            }

            // Get the pointer it maps to.
            let ptr = shmat(id, null_mut(), 0);
            let ptr = match NonNull::new(ptr as *mut i8) {
                Some(ptr) => ptr,
                None => {
                    shmctl(id, IPC_RMID, null_mut());
                    return Err(io::Error::last_os_error());
                }
            };

            Ok(Self {
                id,
                ptr,
                size,
            })
        }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe {
            // Detach the shared memory segment.
            shmdt(self.ptr.as_ptr() as _);

            // Delete the shared memory segment.
            shmctl(self.id, IPC_RMID, null_mut());
        }
    }
}
