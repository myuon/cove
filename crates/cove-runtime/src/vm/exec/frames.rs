//! A task's stack of frames, laid out so that compiled code can push and pop
//! one without a helper ([ADR 0079]).
//!
//! It was a `Vec<Frame>`, and the one thing a `Vec` cannot offer is the thing
//! the native tier's direct call needs: a length that code outside Rust may
//! change. So the length, the storage pointer and the inline path's admission
//! are a [`FrameStack`] — `cove_native`'s `#[repr(C)]` declaration, the contract
//! the code generator stores against — and the storage is a `Vec` whose every
//! element is initialised, of which the first `len` are frames.
//!
//! Everything the machine asks of its frames is a slice question — the top, the
//! length, an iteration, an index — so this derefs to `[Frame]` and the machine
//! reads it exactly as it read the `Vec`. Only the five operations that change
//! the length are methods of their own.
//!
//! [ADR 0079]: ../../../../../docs/adr/0079-a-direct-call-opens-its-frame-in-emitted-code.md

use std::ops::{Deref, DerefMut};

use cove_ir::FunctionId;
use cove_native::{FrameRecord, FrameStack};

use super::Frame;

// The runtime's frame *is* the record compiled code writes: same size, same
// offsets. Asserted rather than assumed, because a field reordered on one side
// would be a frame whose `base` compiled code wrote into its `pc`.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Frame>() == size_of::<FrameRecord>());
    assert!(offset_of!(Frame, base) == offset_of!(FrameRecord, base));
    assert!(offset_of!(Frame, function) == offset_of!(FrameRecord, function));
    assert!(offset_of!(Frame, pc) == offset_of!(FrameRecord, pc));
    assert!(offset_of!(Frame, dst) == offset_of!(FrameRecord, dst));
    assert!(size_of::<FunctionId>() == size_of::<u32>());
};

/// The fewest frames storage is grown to, so a short run reallocates it once.
const FIRST_ROOM: usize = 64;

/// A task's frames. See the module documentation.
pub(super) struct Frames {
    /// What compiled code reads and writes: record 0, the length, and the
    /// length below which it may push.
    published: FrameStack,
    /// The storage. Every element is initialised; `published.len` of them are
    /// frames, and the rest are spare room that a push overwrites.
    storage: Vec<Frame>,
    /// The cap on `published.room` that is not storage: the embedder's
    /// `max_call_depth`, or nought while the inline path is off. See
    /// [`Frames::admit_inline`].
    limit: u64,
}

impl Frames {
    /// No frames, no storage, and the inline path off.
    pub(super) fn new() -> Frames {
        Frames {
            published: FrameStack {
                records: std::ptr::null_mut(),
                len: 0,
                room: 0,
            },
            storage: Vec::new(),
            limit: 0,
        }
    }

    /// Pushes `frame` on top.
    #[inline]
    pub(super) fn push(&mut self, frame: Frame) {
        let len = self.published.len as usize;
        if len == self.storage.len() {
            self.grow();
        }
        self.storage[len] = frame;
        self.published.len += 1;
    }

    /// Takes the top frame off, if there is one.
    #[inline]
    pub(super) fn pop(&mut self) -> Option<Frame> {
        let len = self.published.len as usize;
        if len == 0 {
            return None;
        }
        self.published.len -= 1;
        Some(self.storage[len - 1])
    }

    /// Keeps the bottom `len` frames, and drops the rest.
    #[inline]
    pub(super) fn truncate(&mut self, len: usize) {
        if (len as u64) < self.published.len {
            self.published.len = len as u64;
        }
    }

    /// Drops every frame.
    pub(super) fn clear(&mut self) {
        self.published.len = 0;
    }

    /// Sets the inline path's cap: the configured `max_call_depth`, or `None`
    /// for no cap, or nought to keep compiled code from pushing at all.
    ///
    /// Called by the native tier every time it enters compiled code, with the
    /// limit of the meter that entry runs under — the same number
    /// `Machine::admit_frame` reads at every frame Rust pushes, so a frame
    /// compiled code pushes is one `admit_frame` would have admitted.
    pub(super) fn admit_inline(&mut self, limit: u64) {
        self.limit = limit;
        self.publish_room();
    }

    /// Where compiled code finds this, for [`cove_native::NativeCtx::frames`].
    ///
    /// Valid for as long as `self` does not move, which for a machine's frames
    /// is the whole of a native entry.
    pub(super) fn published(&mut self) -> *mut FrameStack {
        &mut self.published
    }

    /// More storage, in a *new* allocation, with the frames copied across.
    ///
    /// Always a new block, and never `Vec::reserve`: the allocator may extend a
    /// block in place, and then whether the storage moved under a native frame
    /// would be the allocator's choice rather than a fact a test can construct.
    /// Doubling, so the copies are amortised as `Vec`'s own growth is.
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let len = self.published.len as usize;
        let room = (len * 2).max(FIRST_ROOM);
        let mut storage = Vec::with_capacity(room);
        storage.extend_from_slice(&self.storage[..len]);
        storage.resize(room, Frame::EMPTY);
        self.storage = storage;
        self.published.records = self.storage.as_mut_ptr().cast::<FrameRecord>();
        self.publish_room();
    }

    /// `room` is the smaller of the storage and the cap: a record compiled code
    /// pushes neither reallocates nor passes `max_call_depth`.
    fn publish_room(&mut self) {
        self.published.room = (self.storage.len() as u64).min(self.limit);
    }
}

impl Deref for Frames {
    type Target = [Frame];

    #[inline]
    fn deref(&self) -> &[Frame] {
        &self.storage[..self.published.len as usize]
    }
}

impl DerefMut for Frames {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Frame] {
        &mut self.storage[..self.published.len as usize]
    }
}

impl<'f> IntoIterator for &'f Frames {
    type Item = &'f Frame;
    type IntoIter = std::slice::Iter<'f, Frame>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u32) -> Frame {
        Frame {
            base: u64::from(n) * 10,
            function: FunctionId(n),
            pc: n,
            dst: n,
        }
    }

    #[test]
    fn pushes_pops_and_keeps_order_across_growth() {
        let mut frames = Frames::new();
        for n in 0..200 {
            frames.push(frame(n));
        }
        assert_eq!(frames.len(), 200);
        assert_eq!(frames[0].function, FunctionId(0));
        assert_eq!(frames.last().map(|f| f.function), Some(FunctionId(199)));
        assert_eq!(frames.pop().map(|f| f.base), Some(1990));
        frames.truncate(3);
        assert_eq!(frames.len(), 3);
        frames.truncate(10);
        assert_eq!(frames.len(), 3, "a truncation never lengthens");
        frames.clear();
        assert!(frames.pop().is_none());
    }

    #[test]
    fn growth_moves_the_storage_and_republishes_it() {
        let mut frames = Frames::new();
        frames.push(frame(0));
        let before = unsafe { (*frames.published()).records };
        for n in 1..=FIRST_ROOM as u32 {
            frames.push(frame(n));
        }
        let after = unsafe { (*frames.published()).records };
        assert_ne!(before, after, "a growth is a new block, always");
        assert_eq!(after.cast::<Frame>(), frames.as_ptr().cast_mut());
    }

    #[test]
    fn room_is_the_smaller_of_storage_and_limit() {
        let mut frames = Frames::new();
        frames.push(frame(0));
        assert_eq!(
            unsafe { (*frames.published()).room },
            0,
            "off until admitted"
        );
        frames.admit_inline(u64::MAX);
        assert_eq!(unsafe { (*frames.published()).room }, FIRST_ROOM as u64);
        frames.admit_inline(5);
        assert_eq!(unsafe { (*frames.published()).room }, 5);
        frames.admit_inline(0);
        assert_eq!(unsafe { (*frames.published()).room }, 0);
    }
}
