//! Folder comparison page (Location::Compare): two folder trees side by side, usually the two panes of a dual-pane tab.
//! The walk is [`rf_win::foldercmp`] (closing the tab stops it); this file lists what differs, lets the user mark entries and
//! copies them to the left or the right side through the Status Center (conflicts ask, Ctrl+Z undoes). Nothing is deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::cell::Cell;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::format;
use rf_win::fileops::Transfer;
use rf_win::foldercmp::{CompareResult, CompareScan, Diff, Direction, Entry, Side};

use crate::icons;
use crate::t;
use crate::theme::{self, Palette};

/// Rows drawn; the rest are counted below the list.
const LIST_MAX: usize = 2000;

/// Tab and breadcrumb title: "Karşılaştır: A ↔ B".
pub fn title(left: &Path, right: &Path) -> String {
    format!("{}: {} ↔ {}", t!("Karşılaştır"), rf_core::history::display_name(left), rf_core::history::display_name(right))
}

pub struct CompareView {
    left: PathBuf,
    right: PathBuf,
    scan: Option<CompareScan>,
    result: Option<CompareResult>,
    /// Indexes into `result.entries`.
    marked: HashSet<usize>,
    /// Which kinds of difference are listed.
    shown: HashSet<Diff>,
    hover: Option<usize>,
    /// Copies started from here that have not finished; the comparison runs again when the last one does.
    copying: Rc<Cell<usize>>,
    started: std::time::Instant,
    _poll: Option<Task<()>>,
}

impl CompareView {
    pub fn new(left: PathBuf, right: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            scan: Some(CompareScan::start(left.clone(), right.clone())),
            left,
            right,
            result: None,
            marked: HashSet::new(),
            shown: [Diff::OnlyLeft, Diff::OnlyRight, Diff::LeftNewer, Diff::RightNewer, Diff::Conflict].into_iter().collect(),
            hover: None,
            copying: Rc::new(Cell::new(0)),
            started: std::time::Instant::now(),
            _poll: None,
        };
        this.poll(cx);
        this
    }

    pub fn sides(&self) -> (&Path, &Path) {
        (&self.left, &self.right)
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                let Ok(done) = this.update(cx, |v, cx| {
                    let Some(scan) = &v.scan else { return true };
                    if let Some(r) = scan.take() {
                        v.result = Some(r);
                        v.scan = None;
                        cx.notify();
                        return true;
                    }
                    cx.notify();
                    false
                }) else {
                    return;
                };
                if done {
                    return;
                }
            }
        }));
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.result = None;
        self.marked.clear();
        self.hover = None;
        self.started = std::time::Instant::now();
        self.scan = Some(CompareScan::start(self.left.clone(), self.right.clone()));
        self.poll(cx);
        cx.notify();
    }

    fn toggle(&mut self, i: usize, cx: &mut Context<Self>) {
        if !self.marked.remove(&i) {
            self.marked.insert(i);
        }
        cx.notify();
    }

    fn toggle_shown(&mut self, d: Diff, cx: &mut Context<Self>) {
        if !self.shown.remove(&d) {
            self.shown.insert(d);
        }
        // a hidden row is not acted on
        if let Some(r) = &self.result {
            self.marked.retain(|&i| self.shown.contains(&r.entries[i].diff));
        }
        cx.notify();
    }

    /// Marks what copying in `dir` would bring up to date: what only the source side has, and what it has newer.
    fn mark_changes(&mut self, dir: Direction, cx: &mut Context<Self>) {
        if let Some(r) = &self.result {
            self.marked = changes(&r.entries, dir).into_iter().filter(|&i| self.shown.contains(&r.entries[i].diff)).collect();
        }
        cx.notify();
    }

    fn clear_marks(&mut self, cx: &mut Context<Self>) {
        self.marked.clear();
        cx.notify();
    }

    /// Copies the marked entries that the source side has into the other side's matching folders.
    fn copy_marked(&mut self, dir: Direction, cx: &mut Context<Self>) {
        let Some(r) = &self.result else { return };
        let mut idx: Vec<usize> = self.marked.iter().copied().collect();
        idx.sort_unstable();
        let plan = rf_win::foldercmp::copies(&self.left, &self.right, idx.iter().map(|&i| &r.entries[i]), dir);
        if plan.is_empty() {
            return;
        }
        self.marked.clear();
        let me = cx.entity().downgrade();
        for (dest, sources) in plan {
            self.copying.set(self.copying.get() + 1);
            let (me, copying) = (me.clone(), self.copying.clone());
            crate::status_center::transfer_then(cx, Transfer::Copy, sources, dest, move |_, cx| {
                copying.set(copying.get().saturating_sub(1));
                if copying.get() == 0 {
                    let _ = me.update(cx, |v, cx| v.rescan(cx));
                }
            });
        }
        cx.notify();
    }
}

/// The entries that copying in `dir` would bring over: only on the source side, or newer there.
pub(crate) fn changes(entries: &[Entry], dir: Direction) -> Vec<usize> {
    let want = match dir {
        Direction::ToRight => [Diff::OnlyLeft, Diff::LeftNewer],
        Direction::ToLeft => [Diff::OnlyRight, Diff::RightNewer],
    };
    entries.iter().enumerate().filter(|(_, e)| want.contains(&e.diff)).map(|(i, _)| i).collect()
}

/// How many of the marked entries have something to copy in `dir`.
pub(crate) fn copyable(entries: &[Entry], marked: &HashSet<usize>, dir: Direction) -> usize {
    marked
        .iter()
        .filter(|&&i| match dir {
            Direction::ToRight => entries[i].left.is_some(),
            Direction::ToLeft => entries[i].right.is_some(),
        })
        .count()
}

fn diff_label(d: Diff) -> &'static str {
    match d {
        Diff::OnlyLeft => t!("Yalnız solda"),
        Diff::OnlyRight => t!("Yalnız sağda"),
        Diff::LeftNewer => t!("Solda daha yeni"),
        Diff::RightNewer => t!("Sağda daha yeni"),
        Diff::Conflict => t!("Farklı"),
        Diff::Same => t!("Aynı"),
    }
}

fn diff_icon(d: Diff) -> L {
    match d {
        Diff::OnlyLeft | Diff::LeftNewer => L::ArrowRight,
        Diff::OnlyRight | Diff::RightNewer => L::ArrowLeft,
        Diff::Conflict => L::TriangleAlert,
        Diff::Same => L::Check,
    }
}

fn side_text(s: Option<Side>) -> String {
    match s {
        None => "—".into(),
        Some(s) => {
            let when = s
                .modified
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| format::datetime(d.as_millis() as i64))
                .unwrap_or_default();
            if s.is_dir { when } else { format!("{}  {}", format::size(s.size), when) }
        }
    }
}

fn count(n: u64) -> String {
    crate::bench_view::count(n)
}

fn button(id: &'static str, label: String, icon: L, enabled: bool, p: &Palette) -> Stateful<Div> {
    let fg = if enabled { p.text } else { p.text_tertiary };
    h_flex()
        .id(id)
        .gap_1p5()
        .px_2()
        .py_1()
        .items_center()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .whitespace_nowrap()
        .text_color(fg)
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(p.hover)))
        .child(Icon::new(icon).size(px(15.)).text_color(fg))
        .child(label)
}

impl Render for CompareView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let Some(r) = self.result.as_ref() else {
            return self.render_scanning(&p).into_any_element();
        };

        // ---- the two roots
        let roots = h_flex()
            .px_3()
            .pt_2()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(p.text_secondary)
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(format!("{}  {}", t!("Sol:"), self.left.display())))
            .child(Icon::new(L::ArrowLeftRight).size(px(14.)).text_color(p.text_tertiary))
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(format!("{}  {}", t!("Sağ:"), self.right.display())));

        // ---- filter chips: one per kind of difference, with its count
        let mut chips = h_flex().px_3().py_1p5().gap_1().flex_wrap().items_center();
        for (k, d) in [Diff::OnlyLeft, Diff::LeftNewer, Diff::OnlyRight, Diff::RightNewer, Diff::Conflict, Diff::Same].into_iter().enumerate() {
            let on = self.shown.contains(&d);
            chips = chips.child(
                h_flex()
                    .id(("cmp-chip", k))
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .text_xs()
                    .cursor_pointer()
                    .when(on, |c| c.bg(p.selection).text_color(p.text))
                    .when(!on, |c| c.text_color(p.text_tertiary).hover(|s| s.bg(p.hover)))
                    .on_click(cx.listener(move |v, _, _, cx| v.toggle_shown(d, cx)))
                    .child(Icon::new(diff_icon(d)).size(px(12.)))
                    .child(format!("{} ({})", diff_label(d), count(r.count(d) as u64))),
            );
        }

        // ---- actions
        let to_right = copyable(&r.entries, &self.marked, Direction::ToRight);
        let to_left = copyable(&r.entries, &self.marked, Direction::ToLeft);
        let any = !self.marked.is_empty();
        let actions = h_flex()
            .px_3()
            .pb_2()
            .gap_1()
            .flex_wrap()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(button("cmp-sel-r", t!("Sağa gidecekleri seç").to_string(), L::ArrowRight, true, &p).on_click(cx.listener(|v, _, _, cx| v.mark_changes(Direction::ToRight, cx))))
            .child(button("cmp-sel-l", t!("Sola gidecekleri seç").to_string(), L::ArrowLeft, true, &p).on_click(cx.listener(|v, _, _, cx| v.mark_changes(Direction::ToLeft, cx))))
            .child(button("cmp-clear", t!("Seçimi temizle").to_string(), L::Square, any, &p).when(any, |d| d.on_click(cx.listener(|v, _, _, cx| v.clear_marks(cx)))))
            .child(div().flex_1())
            .child(
                button("cmp-copy-r", format!("{} ({})", t!("Sağa kopyala"), count(to_right as u64)), L::CopyPlus, to_right > 0, &p)
                    .when(to_right > 0, |d| d.on_click(cx.listener(|v, _, _, cx| v.copy_marked(Direction::ToRight, cx)))),
            )
            .child(
                button("cmp-copy-l", format!("{} ({})", t!("Sola kopyala"), count(to_left as u64)), L::CopyPlus, to_left > 0, &p)
                    .when(to_left > 0, |d| d.on_click(cx.listener(|v, _, _, cx| v.copy_marked(Direction::ToLeft, cx)))),
            )
            .child(button("cmp-rescan", t!("Yeniden karşılaştır").to_string(), L::RefreshCw, true, &p).on_click(cx.listener(|v, _, _, cx| v.rescan(cx))));

        // ---- the entries
        let mut list = v_flex().id("cmp-list").flex_1().min_h_0().overflow_y_scroll().p_1();
        let mut drawn = 0usize;
        let mut hidden = 0usize;
        for (i, e) in r.entries.iter().enumerate() {
            if !self.shown.contains(&e.diff) {
                continue;
            }
            if drawn >= LIST_MAX {
                hidden += 1;
                continue;
            }
            drawn += 1;
            let on = self.marked.contains(&i);
            let hov = self.hover == Some(i);
            let cat = if e.is_dir() {
                icons::Category::Folder
            } else {
                e.rel.extension().map_or(icons::Category::Other, |x| icons::category_for_ext(&x.to_string_lossy().to_ascii_lowercase()))
            };
            let dcolor = match e.diff {
                Diff::Conflict => p.danger,
                Diff::Same => p.text_tertiary,
                _ => p.accent,
            };
            list = list.child(
                h_flex()
                    .id(("cmp-row", i))
                    .h(theme::ROW_H)
                    .px_2()
                    .gap_2()
                    .items_center()
                    .flex_shrink_0()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.selection))
                    .when(!on && hov, |d| d.bg(p.hover))
                    .on_hover(cx.listener(move |v, is: &bool, _, cx| {
                        let want = is.then_some(i);
                        if *is && v.hover != want || !*is && v.hover == Some(i) {
                            v.hover = want;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |v, _, _, cx| v.toggle(i, cx)))
                    .child(Icon::new(if on { L::SquareCheck } else { L::Square }).size(px(16.)).text_color(if on { p.accent } else { p.text_tertiary }))
                    .child(Icon::new(diff_icon(e.diff)).size(px(14.)).text_color(dcolor))
                    .child(Icon::new(icons::icon_for(cat)).size(px(16.)).text_color(theme::kind_color(cat, p.dark)))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(p.text).child(e.rel.to_string_lossy().into_owned()))
                    .child(div().w(px(200.)).flex_shrink_0().text_xs().text_right().text_color(p.text_secondary).whitespace_nowrap().child(side_text(e.left)))
                    .child(div().w(px(200.)).flex_shrink_0().text_xs().text_right().text_color(p.text_secondary).whitespace_nowrap().child(side_text(e.right))),
            );
        }
        if hidden > 0 {
            list = list.child(div().px_2().py_1().text_xs().text_color(p.text_tertiary).child(format!("+{} {}", count(hidden as u64), t!("öğe daha"))));
        }
        if drawn == 0 {
            let none = if r.entries.iter().all(|e| e.diff == Diff::Same) { t!("Klasörler aynı") } else { t!("Gösterilecek fark yok") };
            list = list.child(div().p_4().text_sm().text_color(p.text_tertiary).child(none));
        }

        let mut foot = format!("{} {}", count(r.entries.len() as u64), t!("öğe karşılaştırıldı"));
        if self.copying.get() > 0 {
            foot.push_str(&format!("  ·  {}", t!("kopyalanıyor; bitince yeniden karşılaştırılacak")));
        }
        if r.unreadable > 0 {
            foot.push_str(&format!("  ·  {} {}", r.unreadable, t!("klasör okunamadı")));
        }
        if r.cancelled {
            foot.push_str(&format!("  ·  {}", t!("tarama yarıda kaldı")));
        }

        v_flex()
            .size_full()
            .child(roots)
            .child(chips)
            .child(actions)
            .child(list)
            .child(div().px_3().py_1p5().border_t_1().border_color(p.divider).text_xs().text_color(p.text_secondary).child(foot))
            .into_any_element()
    }
}

impl CompareView {
    fn render_scanning(&self, p: &Palette) -> impl IntoElement {
        let seen = self.scan.as_ref().map_or(0, |s| s.seen());
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Icon::new(L::ArrowLeftRight).size(px(40.)).text_color(p.accent))
            .child(div().text_color(p.text).child(t!("Klasörler karşılaştırılıyor…")))
            .child(div().text_sm().text_color(p.text_secondary).child(format!("{} {} · {} s", count(seen), t!("öğe"), self.started.elapsed().as_secs())))
            .child(div().text_xs().text_color(p.text_tertiary).child(t!("Sekmeyi kapatmak taramayı durdurur")))
    }
}

#[cfg(test)]
mod tests {
    // (not `use super::*`: the toolkit's glob brings an attribute named `test` that shadows the real one)
    use super::{Diff, Direction, Entry, HashSet, PathBuf, Side, changes, copyable};

    fn e(diff: Diff, l: bool, r: bool) -> Entry {
        let s = Side { is_dir: false, size: 1, modified: None };
        Entry { rel: PathBuf::from("x"), left: l.then_some(s), right: r.then_some(s), diff }
    }

    #[test]
    fn changes_are_what_the_source_side_has_new_or_newer() {
        let es = [e(Diff::OnlyLeft, true, false), e(Diff::LeftNewer, true, true), e(Diff::RightNewer, true, true), e(Diff::OnlyRight, false, true), e(Diff::Same, true, true), e(Diff::Conflict, true, true)];
        assert_eq!(changes(&es, Direction::ToRight), [0, 1]);
        assert_eq!(changes(&es, Direction::ToLeft), [2, 3]);
    }

    #[test]
    fn only_entries_with_a_source_are_copyable() {
        let es = [e(Diff::OnlyLeft, true, false), e(Diff::OnlyRight, false, true), e(Diff::Conflict, true, true)];
        let all: HashSet<usize> = [0, 1, 2].into_iter().collect();
        assert_eq!(copyable(&es, &all, Direction::ToRight), 2);
        assert_eq!(copyable(&es, &all, Direction::ToLeft), 2);
    }
}
