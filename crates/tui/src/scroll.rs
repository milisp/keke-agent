//! Viewport position over the transcript.
//!
//! The position is stored as a *top* line, not as a distance from the bottom.
//! That is the whole point: output appended below a reader who has scrolled up
//! changes the total line count but not the line they are looking at, so the
//! view does not lurch. Following the tail is the absence of a pinned top, not
//! an offset that has to be recomputed on every delta.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrollback {
    /// `None` follows new output; `Some(line)` stays where the reader put it.
    top: Option<usize>,
    height: usize,
    total: usize,
}

impl Scrollback {
    /// Tell the viewport how much there is to show. Called from `draw`, where
    /// wrapping is known; state tests set it directly.
    pub fn measure(&mut self, total: usize, height: usize) {
        self.total = total;
        self.height = height;
        // A pin past the end of a shrunken transcript would show blank space.
        if let Some(top) = self.top {
            self.top = Some(top.min(self.max_top()));
        }
    }

    /// First transcript line to render.
    pub fn offset(&self) -> usize {
        self.top.unwrap_or_else(|| self.max_top())
    }

    /// Whether the viewport is following new output.
    pub fn is_following(&self) -> bool {
        self.top.is_none()
    }

    /// How many lines sit below the viewport. Zero while following.
    pub fn below(&self) -> usize {
        self.total
            .saturating_sub(self.offset().saturating_add(self.height))
    }

    /// The pinned line, for tests asserting that output did not move the view.
    pub fn pinned_top(&self) -> Option<usize> {
        self.top
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let from = self.offset();
        self.top = Some(from.saturating_sub(lines));
    }

    /// Scroll down, resuming follow-the-tail on reaching the bottom.
    ///
    /// Re-pinning at the bottom rather than holding a top equal to `max_top`
    /// is what makes "page down until it stops" leave the reader live again.
    pub fn scroll_down(&mut self, lines: usize) {
        let Some(top) = self.top else { return };
        let next = top.saturating_add(lines);
        self.top = (next < self.max_top()).then_some(next);
    }

    /// Bring `line` into view, moving as little as possible.
    ///
    /// Used to follow a selection that lives inside the transcript — a plan's
    /// selected line — so the highlight a person is moving cannot walk off the
    /// screen it is being read on.
    pub fn reveal(&mut self, line: usize) {
        let top = self.offset();
        if line < top {
            self.top = Some(line);
        } else if self.height > 0 && line >= top + self.height {
            self.top = Some((line + 1 - self.height).min(self.max_top()));
        }
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.page());
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.page());
    }

    /// Jump back to the live tail.
    pub fn follow(&mut self) {
        self.top = None;
    }

    fn page(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }

    fn max_top(&self) -> usize {
        self.total.saturating_sub(self.height)
    }
}
