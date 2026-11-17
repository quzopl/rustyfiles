//! Duplicates page (Location::Duplicates): files with the same contents under a folder or drive.
//! The scan is [`rf_win::dupes`] (below-normal priority, closing the tab stops it); this file draws the groups, lets the user
//! mark copies and moves the marked ones to the Recycle Bin (Ctrl+Z brings them back).

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::format;
use rf_win::dupes::{DupeGroup, DupeResult, DupeScan, Phase};

use crate::fluent_menu::{self, Item, Menu, MenuIcon};
use crate::icons;
use crate::t;
use crate::theme::{self, Palette};

/// Groups drawn (biggest waste first); the rest are counted below the list.
const LIST_MAX: usize = 300;

/// Tab and breadcrumb title: "Yinelenen dosyalar — Belgeler".
pub fn title(root: &std::path::Path) -> String {
    format!("{} — {}", t!("Yinelenen dosyalar"), rf_core::history::display_name(root))
}

pub enum DupesEvent {
    /// Open this path's folder in a normal view with the item selected.
    ShowInFolder(PathBuf),
}

impl EventEmitter<DupesEvent> for DuplicatesView {}

pub struct DuplicatesView {
    root: PathBuf,
    scan: Option<DupeScan>,
    result: Option<DupeResult>,
    /// Files marked to be moved to the Recycle Bin.
    marked: HashSet<PathBuf>,
    hover: Option<PathBuf>,
    started: std::time::Instant,
    _poll: Option<Task<()>>,
}

impl DuplicatesView {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            scan: Some(DupeScan::start(root.clone(), 1)),
            root,
            result: None,
            marked: HashSet::new(),
            hover: None,
            started: std::time::Instant::now(),
            _poll: None,
        };
        this.poll(cx);
        this
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Redraws the progress four times a second; takes the result when it arrives.
    fn poll(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
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
        self.started = std::time::Instant::now();
        self.scan = Some(DupeScan::start(self.root.clone(), 1));
        self.poll(cx);
        cx.notify();
    }

    fn toggle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.marked.remove(&path) {
            self.marked.insert(path);
        }
        cx.notify();
    }

    /// Marks every copy but the oldest of each group.
    fn mark_copies(&mut self, cx: &mut Context<Self>) {
        self.marked = self.result.as_ref().map(|r| marked_copies(&r.groups)).unwrap_or_default();
        cx.notify();
    }

    fn clear_marks(&mut self, cx: &mut Context<Self>) {
        self.marked.clear();
        cx.notify();
    }

    /// Moves the marked files to the Recycle Bin, after a question when a group would lose every copy.
    fn recycle_marked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_empty() {
            return;
        }
        let all_gone = self.result.as_ref().map_or(0, |r| groups_losing_all(&r.groups, &self.marked));
        if all_gone == 0 {
            self.do_recycle(cx);
            return;
        }
        let me = cx.entity().downgrade();
        crate::prompt::show(
            t!("Tüm kopyalar silinsin mi?"),
            Some(format!("{} {}", all_gone, t!("grubun bütün kopyaları işaretli: bu dosyalardan hiçbiri kalmayacak. Geri Dönüşüm Kutusu'ndan geri alınabilirler."))),
            None,
            vec![
                crate::prompt::Button::new(t!("Yine de taşı"), crate::prompt::Kind::Danger, move |_, _, cx| {
                    let _ = me.update(cx, |v, cx| v.do_recycle(cx));
                    Ok(())
                }),
                crate::prompt::Button::cancel(),
            ],
            window,
            cx,
        );
    }

    fn do_recycle(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.marked.drain().collect();
        if let Some(r) = &mut self.result {
            drop_files(&mut r.groups, &paths.iter().cloned().collect());
        }
        crate::status_center::delete(cx, paths, false);
        cx.notify();
    }

    fn context_menu(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let me = cx.entity().downgrade();
        let mut items = Vec::new();
        {
            let path = path.clone();
            items.push(Item::action(t!("Aç"), MenuIcon::Lucide(L::SquareArrowOutUpRight), move |_, cx| {
                crate::folder_view::open_in_background(path.clone(), cx);
            }));
        }
        {
            let (me, path) = (me.clone(), path.clone());
            items.push(Item::action(t!("Klasörde göster"), MenuIcon::Lucide(L::FolderSearch), move |_, cx| {
                let _ = me.update(cx, |_, cx| cx.emit(DupesEvent::ShowInFolder(path.clone())));
            }));
        }
        let on = self.marked.contains(&path);
        items.push(Item::action(if on { t!("İşareti kaldır") } else { t!("Silmek için işaretle") }, MenuIcon::Lucide(L::SquareCheck), move |_, cx| {
            let _ = me.update(cx, |v, cx| v.toggle(path.clone(), cx));
        }));
        fluent_menu::show(Menu { quick: Vec::new(), items }, window.mouse_position(), window, cx);
    }
}

/// Every copy but the first (the oldest) of each group.
pub(crate) fn marked_copies(groups: &[DupeGroup]) -> HashSet<PathBuf> {
    groups.iter().flat_map(|g| g.files.iter().skip(1).map(|f| f.path.clone())).collect()
}

/// How many groups would have no copy left if `marked` went.
pub(crate) fn groups_losing_all(groups: &[DupeGroup], marked: &HashSet<PathBuf>) -> usize {
    groups.iter().filter(|g| g.files.iter().all(|f| marked.contains(&f.path))).count()
}

/// Takes `gone` out of the groups; a group left with fewer than two files is no group of duplicates any more.
pub(crate) fn drop_files(groups: &mut Vec<DupeGroup>, gone: &HashSet<PathBuf>) {
    for g in groups.iter_mut() {
        g.files.retain(|f| !gone.contains(&f.path));
    }
    groups.retain(|g| g.files.len() > 1);
}

fn count(n: u64) -> String {
    crate::bench_view::count(n)
}

fn category_of(path: &std::path::Path) -> icons::Category {
    path.extension().map_or(icons::Category::Other, |e| icons::category_for_ext(&e.to_string_lossy().to_ascii_lowercase()))
}

fn button(id: &'static str, label: String, icon: L, enabled: bool, danger: bool, p: &Palette) -> Stateful<Div> {
    let fg = if !enabled {
        p.text_tertiary
    } else if danger {
        p.danger
    } else {
        p.text
    };
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

impl Render for DuplicatesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let Some(r) = self.result.as_ref() else {
            return self.render_scanning(&p).into_any_element();
        };

        // ---- header: what was found, and what can be done with it
        let marked_n = self.marked.len();
        let marked_bytes: u64 =
            r.groups.iter().flat_map(|g| g.files.iter().filter(|f| self.marked.contains(&f.path)).map(move |_| g.size)).sum();
        let copies: usize = r.groups.iter().map(|g| g.files.len()).sum();
        let summary = if r.groups.is_empty() {
            t!("Yinelenen dosya bulunmadı").to_string()
        } else {
            format!(
                "{} {} · {} {} · {} {}",
                count(r.groups.len() as u64),
                t!("grup"),
                count(copies as u64),
                t!("dosya"),
                format::size(r.wasted()),
                t!("boşaltılabilir")
            )
        };
        let has = !r.groups.is_empty();
        let header = h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(Icon::new(L::Files).size(px(18.)).text_color(p.accent))
            .child(div().flex_1().min_w_0().text_sm().text_color(p.text).overflow_hidden().text_ellipsis().whitespace_nowrap().child(summary))
            .child(
                button("dup-mark", t!("Kopyaları seç").to_string(), L::CopyCheck, has, false, &p)
                    .when(has, |d| d.on_click(cx.listener(|v, _, _, cx| v.mark_copies(cx)))),
            )
            .child(
                button("dup-clear", t!("Seçimi temizle").to_string(), L::Square, marked_n > 0, false, &p)
                    .when(marked_n > 0, |d| d.on_click(cx.listener(|v, _, _, cx| v.clear_marks(cx)))),
            )
            .child(
                button(
                    "dup-recycle",
                    if marked_n > 0 { format!("{} ({}, {})", t!("Geri dönüşüm kutusuna taşı"), count(marked_n as u64), format::size(marked_bytes)) } else { t!("Geri dönüşüm kutusuna taşı").to_string() },
                    L::Trash,
                    marked_n > 0,
                    true,
                    &p,
                )
                .when(marked_n > 0, |d| d.on_click(cx.listener(|v, _, window, cx| v.recycle_marked(window, cx)))),
            )
            .child(button("dup-rescan", t!("Yeniden tara").to_string(), L::RefreshCw, true, false, &p).on_click(cx.listener(|v, _, _, cx| v.rescan(cx))));

        // ---- the groups
        let mut list = v_flex().id("dup-list").flex_1().min_h_0().overflow_y_scroll().p_2().gap_2();
        for (gi, g) in r.groups.iter().take(LIST_MAX).enumerate() {
            let mut card = v_flex().flex_shrink_0().rounded(theme::RADIUS_SM).border_1().border_color(p.border).overflow_hidden();
            card = card.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .bg(p.pressed)
                    .text_xs()
                    .text_color(p.text_secondary)
                    .child(div().flex_1().child(format!("{} {} · {} {}", g.files.len(), t!("kopya"), format::size(g.size), t!("her biri"))))
                    .child(div().child(format!("{} {}", format::size(g.wasted()), t!("boşaltılabilir")))),
            );
            for (fi, f) in g.files.iter().enumerate() {
                let path = f.path.clone();
                let on = self.marked.contains(&path);
                let hov = self.hover.as_ref() == Some(&path);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dir = path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                let when = f
                    .modified
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| format::datetime(d.as_millis() as i64))
                    .unwrap_or_default();
                let cat = category_of(&path);
                let (p_click, p_hover, p_menu, p_open) = (path.clone(), path.clone(), path.clone(), path.clone());
                card = card.child(
                    h_flex()
                        .id(("dup-row", gi * 10_000 + fi))
                        .h(theme::ROW_H)
                        .px_2()
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .cursor_pointer()
                        .when(on, |d| d.bg(p.selection))
                        .when(!on && hov, |d| d.bg(p.hover))
                        .on_hover(cx.listener(move |v, is: &bool, _, cx| {
                            if *is && v.hover.as_ref() != Some(&p_hover) {
                                v.hover = Some(p_hover.clone());
                                cx.notify();
                            } else if !*is && v.hover.as_ref() == Some(&p_hover) {
                                v.hover = None;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |v, ev: &ClickEvent, _, cx| {
                            if ev.click_count() >= 2 {
                                crate::folder_view::open_in_background(p_open.clone(), cx);
                            } else {
                                v.toggle(p_click.clone(), cx);
                            }
                        }))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |v, _, window, cx| v.context_menu(p_menu.clone(), window, cx)))
                        .child(Icon::new(if on { L::SquareCheck } else { L::Square }).size(px(16.)).text_color(if on { p.accent } else { p.text_tertiary }))
                        .child(Icon::new(icons::icon_for(cat)).size(px(16.)).text_color(theme::kind_color(cat, p.dark)))
                        .child(div().flex_shrink_0().max_w(px(260.)).overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(p.text).child(name))
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_xs().text_color(p.text_tertiary).child(dir))
                        .when(fi == 0, |d| d.child(div().flex_shrink_0().text_xs().text_color(p.text_secondary).child(t!("en eski"))))
                        .child(div().w(px(120.)).flex_shrink_0().text_xs().text_right().text_color(p.text_secondary).child(when)),
                );
            }
            list = list.child(card);
        }
        let hidden = r.groups.len().saturating_sub(LIST_MAX);
        if hidden > 0 {
            list = list.child(div().px_2().py_1().text_xs().text_color(p.text_tertiary).child(format!("+{} {}", count(hidden as u64), t!("grup daha"))));
        }

        // ---- footer: how the scan went
        let mut foot = format!("{}  ·  {} {}", self.root.to_string_lossy(), count(r.files), t!("dosya tarandı"));
        if r.unreadable > 0 {
            foot.push_str(&format!("  ·  {} {}", count(r.unreadable), t!("dosya okunamadı")));
        }
        if r.skipped_dirs > 0 {
            foot.push_str(&format!("  ·  {} {}", r.skipped_dirs, t!("klasör okunamadı")));
        }
        if r.cancelled {
            foot.push_str(&format!("  ·  {}", t!("tarama yarıda kaldı")));
        }

        v_flex()
            .size_full()
            .child(header)
            .child(list)
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(p.divider)
                    .text_xs()
                    .text_color(p.text_secondary)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(foot),
            )
            .into_any_element()
    }
}

impl DuplicatesView {
    fn render_scanning(&self, p: &Palette) -> impl IntoElement {
        let secs = self.started.elapsed().as_secs();
        let line = match &self.scan {
            Some(s) => {
                let pr = s.progress();
                match pr.phase() {
                    Phase::Walking => format!("{} {} · {} s", count(pr.entries()), t!("öğe"), secs),
                    Phase::Comparing | Phase::Done => {
                        use std::sync::atomic::Ordering::Relaxed;
                        format!(
                            "{} {} / {} · {} · {} s",
                            t!("Karşılaştırılıyor:"),
                            count(pr.hashed.load(Relaxed)),
                            count(pr.to_hash.load(Relaxed)),
                            format::size(pr.bytes.load(Relaxed)),
                            secs
                        )
                    }
                }
            }
            None => String::new(),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Icon::new(L::Files).size(px(40.)).text_color(p.accent))
            .child(div().text_color(p.text).child(t!("Yinelenen dosyalar aranıyor…")))
            .child(div().text_sm().text_color(p.text_secondary).child(line))
            .child(div().text_xs().text_color(p.text_tertiary).child(t!("Sekmeyi kapatmak taramayı durdurur")))
    }
}

#[cfg(test)]
mod tests {
    // (not `use super::*`: the toolkit's glob brings an attribute named `test` that shadows the real one)
    use super::{DupeGroup, HashSet, PathBuf, drop_files, groups_losing_all, marked_copies};
    use rf_win::dupes::DupeFile;

    fn group(size: u64, names: &[&str]) -> DupeGroup {
        DupeGroup { size, files: names.iter().map(|n| DupeFile { path: PathBuf::from(n), modified: None }).collect() }
    }

    #[test]
    fn copies_are_all_but_the_oldest_of_each_group() {
        let g = vec![group(10, &["a", "b", "c"]), group(5, &["x", "y"])];
        let m = marked_copies(&g);
        assert_eq!(m, ["b", "c", "y"].iter().map(PathBuf::from).collect());
        assert_eq!(groups_losing_all(&g, &m), 0, "the oldest copy of each group stays");
    }

    #[test]
    fn a_group_with_every_copy_marked_is_counted() {
        let g = vec![group(10, &["a", "b"]), group(5, &["x", "y"])];
        let m: HashSet<PathBuf> = ["a", "b", "x"].iter().map(PathBuf::from).collect();
        assert_eq!(groups_losing_all(&g, &m), 1);
    }

    #[test]
    fn recycled_files_leave_the_list_and_a_group_of_one_goes() {
        let mut g = vec![group(10, &["a", "b", "c"]), group(5, &["x", "y"])];
        drop_files(&mut g, &["b", "y"].iter().map(PathBuf::from).collect());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), vec![PathBuf::from("a"), PathBuf::from("c")]);
    }
}
