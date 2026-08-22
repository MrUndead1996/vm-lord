//! The bounded queue, which sits *before* the encoder.
//!
//! Discarding an encoded delta would be silent corruption: the next delta
//! would be encoded against a frame the viewer never received, applied to the
//! wrong base, and nothing anywhere would detect it -- no error, no
//! `RequestKeyframe`, just a picture that drifts. So what a slow socket
//! discards is a *captured* frame, and encoding happens when the caller asks
//! for a payload. That is what makes the encoder's reference frame, by
//! construction, the last payload the caller was handed.
//!
//! Every slot here is latest-wins. The frame slot keeps one frame, the cursor
//! keeps one bitmap and one position, and a keyframe request is a flag rather
//! than something queued.

use crate::geometry::Rect;

/// What has been captured and not yet encoded.
pub(crate) struct Staging {
    frame: Vec<u32>,
    frame_pending: bool,
    /// Where the staged frame may differ from the reference, accumulated
    /// across every submission since the last payload: a frame displaced
    /// before it was encoded still carried changes, and its hint is the only
    /// record of where they were. `None` means "somewhere".
    hint: Option<Vec<Rect>>,
    cursor_image: Vec<u8>,
    cursor_image_pending: bool,
    cursor_position: Vec<u8>,
    cursor_position_pending: bool,
    keyframe_requested: bool,
}

impl Staging {
    /// Staging for a frame of `pixels` pixels.
    pub(crate) fn new(pixels: usize) -> Self {
        Self {
            frame: vec![0; pixels],
            frame_pending: false,
            hint: Some(Vec::new()),
            cursor_image: Vec::new(),
            cursor_image_pending: false,
            cursor_position: Vec::new(),
            cursor_position_pending: false,
            keyframe_requested: false,
        }
    }

    /// The frame buffer, to be written in place: a captured frame displaces
    /// the one before it rather than allocating beside it.
    pub(crate) fn frame_mut(&mut self) -> &mut [u32] {
        &mut self.frame
    }

    /// The frame as it now stands, whether or not it is pending.
    ///
    /// A keyframe asked for while nothing new has been captured is encoded
    /// from this: a viewer that lost synchronisation must not wait for the
    /// guest to repaint something.
    pub(crate) fn frame(&self) -> &[u32] {
        &self.frame
    }

    /// Marks the frame buffer as holding something worth sending.
    pub(crate) fn stage_frame(&mut self, damage: Option<&[Rect]>) {
        match (damage, self.hint.as_mut()) {
            (Some(rects), Some(hint)) => hint.extend_from_slice(rects),
            (Some(_), None) => {}
            (None, _) => self.hint = None,
        }

        self.frame_pending = true;
    }

    /// Takes the pending frame's hint, if a frame is pending at all.
    pub(crate) fn take_frame(&mut self) -> Option<Option<Vec<Rect>>> {
        if !self.frame_pending {
            return None;
        }

        self.frame_pending = false;
        Some(self.hint.replace(Vec::new()))
    }

    /// Replaces the pending cursor bitmap.
    pub(crate) fn cursor_image_mut(&mut self) -> &mut Vec<u8> {
        &mut self.cursor_image
    }

    /// Marks the cursor bitmap as worth sending.
    pub(crate) fn stage_cursor_image(&mut self) {
        self.cursor_image_pending = true;
    }

    /// Clears the cursor bitmap's pending flag, and says whether it was set.
    ///
    /// The bytes come back from [`Staging::cursor_image`] rather than from
    /// here: a payload borrows them for as long as the caller holds it, and
    /// one `&mut self` cannot hand out that borrow and still be asked about
    /// the position afterwards.
    pub(crate) fn take_cursor_image(&mut self) -> bool {
        let pending = self.cursor_image_pending;
        self.cursor_image_pending = false;
        pending
    }

    /// The cursor bitmap as it now stands.
    pub(crate) fn cursor_image(&self) -> &[u8] {
        &self.cursor_image
    }

    /// Replaces the pending cursor position.
    pub(crate) fn cursor_position_mut(&mut self) -> &mut Vec<u8> {
        &mut self.cursor_position
    }

    /// Marks the cursor position as worth sending.
    pub(crate) fn stage_cursor_position(&mut self) {
        self.cursor_position_pending = true;
    }

    /// Clears the cursor position's pending flag, and says whether it was set.
    pub(crate) fn take_cursor_position(&mut self) -> bool {
        let pending = self.cursor_position_pending;
        self.cursor_position_pending = false;
        pending
    }

    /// The cursor position as it now stands.
    pub(crate) fn cursor_position(&self) -> &[u8] {
        &self.cursor_position
    }

    /// Records that the viewer asked for a keyframe.
    pub(crate) fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    /// Whether a keyframe was asked for.
    pub(crate) fn keyframe_requested(&self) -> bool {
        self.keyframe_requested
    }

    /// Clears the request, once a keyframe has actually been produced.
    pub(crate) fn keyframe_sent(&mut self) {
        self.keyframe_requested = false;
    }
}
