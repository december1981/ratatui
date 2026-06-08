use crate::buffer::{Buffer, Cell, CellDiffOption, CellWidth};
use crate::layout::Rect;

/// A zero-allocation iterator over the differences between two buffers of the same width.
///
/// Yields `(x, y, &Cell)` tuples for each cell in `next` that differs from the corresponding cell
/// in `prev`. Handles multi-width characters (including VS16 emoji trailing cells) and
/// [`CellDiffOption`] directives.
#[derive(Debug)]
pub struct BufferDiff<'prev, 'next> {
    /// The next (current) buffer's cells.
    next: &'next [Cell],
    /// The previous buffer's cells.
    prev: &'prev [Cell],
    /// Buffer width (for `pos_of` calculation).
    area: Rect,
    /// Current position in the flat cell array.
    pos: usize,
    /// Remaining trailing cells physically covered by a preceding multi-width glyph in `next`.
    /// These are normally suppressed; the terminal clears them when it draws the wide glyph.
    to_skip: usize,
    /// Cells that must be redrawn because the *wider* of the previous/next glyph at an earlier
    /// column painted over them. This mirrors the classic ratatui `invalidated` counter that was
    /// dropped by the cell-diff-options change (#1605): without it, shrinking a wide glyph (e.g. a
    /// full-width `＋` or a styled background) leaves the trailing cell un-cleared because the new
    /// content there happens to equal what we last drew.
    invalidated: usize,
    /// Whether the active `to_skip` region originates from a VS16 (U+FE0F) presentation sequence.
    /// Such trailing cells are emitted when their symbol changes, working around terminals that
    /// fail to clear them automatically.
    vs16_trailing: bool,
}

impl<'prev, 'next> BufferDiff<'prev, 'next> {
    /// Creates a new iterator over the differences between `prev` and `next` terminal cells.
    ///
    /// Heights may differ; the iterator uses the minimum of the two.
    ///
    /// # Panics
    ///
    /// Panics if the buffers have different `x`, `y`, or `width` values.
    pub(crate) fn new(prev: &'prev Buffer, next: &'next Buffer) -> Self {
        assert!(
            prev.area.x == next.area.x
                && prev.area.y == next.area.y
                && prev.area.width == next.area.width,
            "buffer areas must have the same x, y, and width: prev={:?}, next={:?}",
            prev.area,
            next.area,
        );

        let mut area = prev.area;
        area.height = area.height.min(next.area.height);

        Self {
            next: &next.content,
            prev: &prev.content,
            area,
            pos: 0,
            to_skip: 0,
            invalidated: 0,
            vs16_trailing: false,
        }
    }

    /// Converts a flat index to (x, y) coordinates.
    const fn pos_of(&self, index: usize) -> (u16, u16) {
        let w = self.area.width as usize;

        let x = index % w + self.area.x as usize;
        let y = index / w + self.area.y as usize;

        (x as u16, y as u16)
    }
}

impl<'next> Iterator for BufferDiff<'_, 'next> {
    type Item = (u16, u16, &'next Cell);

    fn next(&mut self) -> Option<Self::Item> {
        let len = self.next.len().min(self.prev.len());
        while self.pos < len {
            let i = self.pos;
            self.pos += 1;

            let current = &self.next[i];
            let previous = &self.prev[i];

            // Decide whether this cell needs to be emitted, using the skip/invalidation state
            // carried in from earlier columns (i.e. before folding in this cell's own width).
            let emit = if is_skip(current) {
                // Caller-managed cell: never emitted, but still participates in width accounting.
                false
            } else if self.to_skip > 0 {
                // Inside the region physically covered by a preceding wide glyph in `next`.
                // Normally suppressed (the terminal clears it when drawing the wide glyph), but
                // some terminals fail to clear the trailing cell of a VS16 emoji, so emit it when
                // its symbol changed. The style of a hidden trailing cell is not visible, so a
                // style-only change must not trigger an update (it can mis-position the cursor).
                self.vs16_trailing && previous.symbol() != current.symbol()
            } else {
                match current.diff_option {
                    CellDiffOption::ForcedWidth(_) => current != previous,
                    CellDiffOption::AlwaysUpdate => true,
                    // `Skip` is handled by `is_skip` above; only `None` reaches this arm.
                    _ => current != previous || self.invalidated > 0,
                }
            };

            // Fold this cell into the skip/invalidation state for the following columns.
            let width = current.cell_width() as usize;
            if self.to_skip > 0 {
                self.to_skip -= 1;
                if self.to_skip == 0 {
                    self.vs16_trailing = false;
                }
            } else {
                self.to_skip = width.saturating_sub(1);
                self.vs16_trailing =
                    width > 1 && current.symbol().chars().any(|c| c == '\u{FE0F}');
            }
            // The previous glyph may have been wider than the next one; the cells it painted over
            // must be redrawn even if their new content matches what we last drew there.
            let affected = width.max(previous.cell_width() as usize);
            self.invalidated = affected.max(self.invalidated).saturating_sub(1);

            if emit {
                let (x, y) = self.pos_of(i);
                return Some((x, y, &self.next[i]));
            }
        }

        None
    }
}

/// Returns `true` if this cell should be skipped during diffing.
#[allow(deprecated)]
const fn is_skip(cell: &Cell) -> bool {
    matches!(cell.diff_option, CellDiffOption::Skip)
        || (cell.skip && matches!(cell.diff_option, CellDiffOption::None))
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::num::NonZeroU16;

    use compact_str::CompactString;

    use super::*;
    use crate::buffer::Buffer;
    use crate::layout::Rect;

    #[test]
    fn empty_buffers_yield_no_diffs() {
        let rect = Rect::new(0, 0, 5, 1);
        let buf = Buffer::empty(rect);
        let diff: Vec<_> = BufferDiff::new(&buf, &buf).collect();
        assert!(diff.is_empty());
    }

    #[test]
    fn identical_buffers_yield_no_diffs() {
        let buf = Buffer::with_lines(["hello"]);
        let diff: Vec<_> = BufferDiff::new(&buf, &buf).collect();
        assert!(diff.is_empty());
    }

    #[test]
    fn single_cell_change() {
        let prev = Buffer::with_lines(["hello"]);
        let next = Buffer::with_lines(["hallo"]);
        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].0, 1); // x
        assert_eq!(diff[0].1, 0); // y
        assert_eq!(diff[0].2.symbol(), "a");
    }

    #[test]
    fn all_cells_changed() {
        let prev = Buffer::with_lines(["aaa"]);
        let next = Buffer::with_lines(["bbb"]);
        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 3);
    }

    #[test]
    fn skip_cells_are_skipped() {
        let prev = Buffer::with_lines(["abc"]);
        let mut next = Buffer::with_lines(["xyz"]);
        next.content[1].diff_option = CellDiffOption::Skip;

        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].2.symbol(), "x");
        assert_eq!(diff[1].2.symbol(), "z");
    }

    #[test]
    fn always_update_cells_are_emitted_even_when_identical() {
        let mut prev = Buffer::with_lines(["abc"]);
        prev.content[1].diff_option = CellDiffOption::AlwaysUpdate;

        let mut next = Buffer::with_lines(["abc"]);
        next.content[1].diff_option = CellDiffOption::AlwaysUpdate;

        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].0, 1);
        assert_eq!(diff[0].1, 0);
        assert_eq!(diff[0].2.symbol(), "b");
    }

    #[test]
    fn forced_width_skips_trailing() {
        let prev = Buffer::with_lines(["abcd"]);
        let mut next = Buffer::with_lines(["xbcd"]);
        next.content[0].diff_option = CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap());

        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].2.symbol(), "x");
    }

    #[test]
    fn vs16_trailing_cell_unchanged() {
        use crate::style::{Color, Style};

        let rect = Rect::new(0, 0, 4, 1);
        let mut prev = Buffer::empty(rect);
        prev.set_string(0, 0, "⌨️", Style::new());
        prev.set_string(2, 0, "ab", Style::new());

        let mut next = Buffer::empty(rect);
        next.set_string(0, 0, "⌨️", Style::new().fg(Color::Red));
        next.set_string(2, 0, "ab", Style::new());

        // Only the main emoji cell (0,0) differs (different style);
        // the trailing cell (1,0) is identical in both buffers.
        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].0, 0);
        assert_eq!(diff[0].1, 0);
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_skip_field_is_respected() {
        let prev = Buffer::with_lines(["abc"]);
        let mut next = Buffer::with_lines(["xyz"]);
        next.content[1].skip = true;

        let diff: CompactString = BufferDiff::new(&prev, &next)
            .map(|(_, _, cell)| cell.symbol())
            .collect();

        assert_eq!(diff, "xz");
    }

    #[test]
    #[allow(deprecated)]
    fn forced_width_takes_precedence_over_deprecated_skip() {
        let prev = Buffer::with_lines(["abcd"]);
        let mut next = Buffer::with_lines(["xbcd"]);
        next.content[0].skip = true;
        next.content[0].diff_option = CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap());

        // ForcedWidth wins over skip=true, so the cell is diffed with forced width
        let diff: CompactString = BufferDiff::new(&prev, &next)
            .map(|(_, _, cell)| cell.symbol())
            .collect();

        assert_eq!(diff, "x");
    }

    #[test]
    fn shrinking_wide_glyph_clears_trailing_cell() {
        // Regression for https://github.com/ratatui/ratatui/issues/2585 (introduced by #1605):
        // the cell-diff-options refactor dropped the classic `invalidated` counter, so when a
        // multi-width glyph (here the full-width plus ＋, U+FF0B) is replaced by narrower content,
        // the trailing cell it physically painted over was left un-cleared. Because that trailing
        // cell is blank in both buffers (`current == previous`), only the previous glyph's width
        // can force the redraw — exactly what `invalidated` tracks.
        let rect = Rect::new(0, 0, 2, 1);
        let mut prev = Buffer::empty(rect);
        prev.set_string(0, 0, "＋", crate::style::Style::new());
        assert_eq!(prev.content[0].symbol(), "＋");
        // The trailing cell is reset to a blank space when the wide glyph is written.
        assert_eq!(prev.content[1].symbol(), " ");

        // Next frame clears the glyph: both cells are blank.
        let next = Buffer::empty(rect);
        assert_eq!(next.content[1], prev.content[1]); // trailing cell is unchanged

        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert_eq!(diff.len(), 2, "both columns must be redrawn, got {diff:?}");
        assert_eq!((diff[0].0, diff[0].1), (0, 0));
        assert_eq!(diff[0].2.symbol(), " ");
        // The trailing cell (1,0) must be emitted even though it is identical in both buffers,
        // otherwise the right half of the old glyph (and its background) lingers on screen.
        assert_eq!((diff[1].0, diff[1].1), (1, 0));
        assert_eq!(diff[1].2.symbol(), " ");
    }

    #[test]
    fn shrinking_wide_glyph_clears_trailing_background() {
        use crate::style::{Color, Style};

        // Same regression, framed as the reported symptom: a styled background painted across a
        // wide glyph must be cleared from the trailing cell when the glyph shrinks away.
        let rect = Rect::new(0, 0, 2, 1);
        let mut prev = Buffer::empty(rect);
        prev.set_string(0, 0, "＋", Style::new().bg(Color::Blue));

        let next = Buffer::empty(rect); // blank, default background

        let diff: Vec<_> = BufferDiff::new(&prev, &next).collect();
        assert!(
            diff.iter().any(|(x, y, _)| *x == 1 && *y == 0),
            "trailing cell (1,0) must be redrawn to clear the leftover background, got {diff:?}"
        );
    }

    #[test]
    #[should_panic(expected = "buffer areas must have the same x, y, and width")]
    fn mismatched_widths_panics() {
        let prev = Buffer::empty(Rect::new(0, 0, 5, 1));
        let next = Buffer::empty(Rect::new(0, 0, 10, 1));
        BufferDiff::new(&prev, &next);
    }
}
