//! Shared selection model for TUI lists and menus: a wrapping cursor plus the
//! scroll offset that keeps it inside a fixed-height viewport. Every
//! list/menu widget should route its index and scroll math through these
//! pure functions instead of reimplementing modulo/saturating arithmetic
//! per widget — that per-widget reimplementation is what let some lists
//! wrap or scroll and others silently not.
//!
//! Frame-free and state-free by design (mirrors `log_scrollbar_geometry` in
//! `ui::panel::log_fallback`): callers own `index`/`scroll` as plain fields
//! and pass the current `len`/`visible_rows` in on every call, so the maths
//! is exercised here without a terminal or a stored widget struct.

/// Move `index` one step through a list of `len` items, wrapping at either
/// end: up from the first item lands on the last, down from the last lands
/// on the first. A no-op (returns `0`) on an empty list; stays at `0` for a
/// single-item list.
pub fn move_index(index: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        if index + 1 >= len {
            0
        } else {
            index + 1
        }
    } else {
        index.checked_sub(1).unwrap_or(len - 1)
    }
}

/// Recompute the scroll offset so `index` (into a list of `len` items) stays
/// inside a `visible_rows`-tall window, sliding by exactly enough to bring
/// it back into view. A list that already fits (`len <= visible_rows`)
/// never scrolls, regardless of a stale incoming `scroll`.
pub fn clamp_scroll(index: usize, scroll: usize, len: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 || len <= visible_rows {
        return 0;
    }
    let max_scroll = len - visible_rows;
    let scroll = if index < scroll {
        index
    } else if index >= scroll + visible_rows {
        index + 1 - visible_rows
    } else {
        scroll
    };
    scroll.min(max_scroll)
}

/// Move the selection one step (wrapping) and reclamp the scroll offset in
/// one call — the combination every list/menu key handler needs.
pub fn move_selection(
    index: usize,
    scroll: usize,
    len: usize,
    visible_rows: usize,
    forward: bool,
) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let index = move_index(index, len, forward);
    let scroll = clamp_scroll(index, scroll, len, visible_rows);
    (index, scroll)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── move_index ──────────────────────────────────────────────

    #[test]
    fn up_from_first_wraps_to_last() {
        assert_eq!(move_index(0, 5, false), 4);
    }

    #[test]
    fn down_from_last_wraps_to_first() {
        assert_eq!(move_index(4, 5, true), 0);
    }

    #[test]
    fn down_from_middle_advances_by_one() {
        assert_eq!(move_index(2, 5, true), 3);
    }

    #[test]
    fn up_from_middle_retreats_by_one() {
        assert_eq!(move_index(2, 5, false), 1);
    }

    #[test]
    fn empty_list_move_is_noop() {
        assert_eq!(move_index(0, 0, true), 0);
        assert_eq!(move_index(0, 0, false), 0);
    }

    #[test]
    fn single_item_move_stays_at_zero() {
        assert_eq!(move_index(0, 1, true), 0);
        assert_eq!(move_index(0, 1, false), 0);
    }

    // ── clamp_scroll ────────────────────────────────────────────

    #[test]
    fn clamp_scroll_stays_put_when_selection_already_visible() {
        assert_eq!(clamp_scroll(2, 0, 20, 5), 0);
    }

    #[test]
    fn clamp_scroll_follows_selection_past_bottom_edge() {
        // visible_rows=5 shows rows [0,5); selecting row 5 must slide by one.
        assert_eq!(clamp_scroll(5, 0, 20, 5), 1);
    }

    #[test]
    fn clamp_scroll_follows_selection_past_top_edge() {
        assert_eq!(clamp_scroll(2, 3, 20, 5), 2);
    }

    #[test]
    fn clamp_scroll_short_list_never_scrolls() {
        // n <= h: offset stays 0 regardless of index or a stale scroll.
        assert_eq!(clamp_scroll(4, 3, 5, 5), 0);
        assert_eq!(clamp_scroll(0, 0, 3, 5), 0);
    }

    #[test]
    fn clamp_scroll_zero_viewport_is_zero() {
        assert_eq!(clamp_scroll(3, 2, 10, 0), 0);
    }

    // ── move_selection (index + scroll together) ───────────────

    #[test]
    fn move_selection_down_past_last_visible_row_advances_offset_by_one() {
        // 10 items, viewport 5, selection sits on the last visible row (4).
        let (index, scroll) = move_selection(4, 0, 10, 5, true);
        assert_eq!(index, 5);
        assert_eq!(scroll, 1, "offset must advance by exactly one");
        assert!(
            index >= scroll && index < scroll + 5,
            "selection must stay inside the viewport"
        );
    }

    #[test]
    fn move_selection_up_above_first_visible_row_retreats_offset_by_one() {
        // Mirror case: selection sits on the first visible row (1) with offset 1.
        let (index, scroll) = move_selection(1, 1, 10, 5, false);
        assert_eq!(index, 0);
        assert_eq!(scroll, 0, "offset must retreat by exactly one");
        assert!(index >= scroll && index < scroll + 5);
    }

    #[test]
    fn move_selection_short_list_offset_stays_zero_for_every_move() {
        let mut index = 0;
        let mut scroll = 0;
        for forward in [true, true, true, false, false, true] {
            let (i, s) = move_selection(index, scroll, 3, 5, forward);
            index = i;
            scroll = s;
            assert_eq!(scroll, 0, "n <= h must never scroll");
        }
    }

    #[test]
    fn move_selection_empty_list_is_noop_no_panic() {
        assert_eq!(move_selection(0, 0, 0, 5, true), (0, 0));
        assert_eq!(move_selection(0, 0, 0, 5, false), (0, 0));
    }

    #[test]
    fn move_selection_single_item_stays_at_zero_no_panic() {
        assert_eq!(move_selection(0, 0, 1, 5, true), (0, 0));
        assert_eq!(move_selection(0, 0, 1, 5, false), (0, 0));
    }

    #[test]
    fn move_selection_wrap_from_last_to_first_shows_first_item_not_last_viewport() {
        // 20 items, viewport 5: sitting at the last item with the view
        // scrolled to the bottom page, wrapping down must jump the view back
        // to the top — a naive `clamp_scroll` that doesn't special-case the
        // wrapped index would leave the offset on the last page instead.
        let (index, scroll) = move_selection(19, 15, 20, 5, true);
        assert_eq!(index, 0);
        assert_eq!(
            scroll, 0,
            "wrapping to the first item must show the first page"
        );
    }

    #[test]
    fn move_selection_wrap_from_first_to_last_shows_last_page() {
        let (index, scroll) = move_selection(0, 0, 20, 5, false);
        assert_eq!(index, 19);
        assert_eq!(
            scroll, 15,
            "wrapping to the last item must show the last page"
        );
    }
}
