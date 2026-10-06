//! 最小的行级 diff：**为了让人在批准前看清"文件会变成什么样"**。
//!
//! ## 为什么需要它
//!
//! 在此之前，审批提示上显示的是工具参数的 JSON：
//!
//! ```text
//! 批准 edit_file？
//!   {"path":"notes/a.md","old_string":"旧的一行","new_string":"新的一行"}
//! ```
//!
//! 那是**参数**，不是**后果**。而项目自己在 `specifier` 的注释里写过
//! 这个担忧：「审批提示上显示的是 `notes\a.md`，而工具真正动的是
//! `<cwd>\notes\a.md`——**人在批准一个自己没看清的东西**。」
//!
//! 同一句话适用于这里：JSON 参数不是"你要改的东西"。
//!
//! ## 为什么自己写而不引依赖
//!
//! `similar` / `diff` 这类 crate 都能做，但：
//!
//! - 我们只要**行级、带少量上下文**这一种输出，不需要字符级、不需要
//!   各种格式、不需要 Myers 的完整实现
//! - 项目的依赖纪律是"先量出代价再引"（D9 被 D20/D32 收窄过两次）
//!
//! 所以这里用 LCS 动态规划，几十行，**能被测试完全覆盖**。
//! 大文件的上限由调用方控制（见 [`MAX_LINES`]）。
//!
//! ## 确定性
//!
//! 同样的输入必须给同样的输出。这不只是为了测试好写——
//! **审批提示每次长得不一样会让人以为改动也不一样。**

/// 参与 diff 的行数上限。超过就退化成"只说改了多少行"。
///
/// 审批提示是要给人看的：几千行的 diff 没人读得完，
/// 而真正会被批准的是"看起来没问题的那个"。
pub const MAX_LINES: usize = 400;

/// 上下文行数（改动前后各留几行）。
pub const CONTEXT: usize = 3;

/// 渲染出来的行数上限。再多就截断并说明。
pub const MAX_RENDERED: usize = 60;

/// 一行差异。
#[derive(Debug, Clone, PartialEq)]
pub enum LineDiff {
    /// 两边都有。
    Same(String),
    /// 只在新版本里。
    Added(String),
    /// 只在旧版本里。
    Removed(String),
}

/// 算两段文本的行级差异。
///
/// 任一边超过 [`MAX_LINES`] 就返回 `None`——调用方应当退化成
/// "只说改了多少行"，而不是渲染一个没人读得完的东西。
pub fn diff_lines(old: &str, new: &str) -> Option<Vec<LineDiff>> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        return None;
    }

    // LCS 长度表。O(n*m) 空间——上限 400 行时是 16 万个格子，
    // 一次审批提示的开销可以忽略。
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(LineDiff::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(LineDiff::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(LineDiff::Added(b[j].to_string()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| LineDiff::Removed(l.to_string())));
    out.extend(b[j..].iter().map(|l| LineDiff::Added(l.to_string())));
    Some(out)
}

/// 一段 diff 的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStat {
    pub added: usize,
    pub removed: usize,
}

impl DiffStat {
    pub fn of(diff: &[LineDiff]) -> Self {
        let mut s = Self::default();
        for d in diff {
            match d {
                LineDiff::Added(_) => s.added += 1,
                LineDiff::Removed(_) => s.removed += 1,
                LineDiff::Same(_) => {}
            }
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// "加 3 行、删 1 行"。
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "没有变化".to_string();
        }
        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(format!("加 {} 行", self.added));
        }
        if self.removed > 0 {
            parts.push(format!("删 {} 行", self.removed));
        }
        parts.join("、")
    }
}

/// 渲染成给人看的样子：**只留改动附近的几行**。
///
/// 全量渲染没用——一个 300 行的文件只改一行的话，人要在 300 行里
/// 找那一行。只留上下文的话，一眼就能看到"到底动了什么"。
pub fn render(old: &str, new: &str, label: &str) -> String {
    let Some(diff) = diff_lines(old, new) else {
        return format!(
            "{label}：文件太大（超过 {MAX_LINES} 行），看不出逐行差异。\
             请自行确认后再批准。"
        );
    };
    let stat = DiffStat::of(&diff);
    if stat.is_empty() {
        return format!("{label}：内容没有变化。");
    }

    // 标记哪些下标要显示：改动本身 + 前后各 CONTEXT 行
    let changed: Vec<usize> = diff
        .iter()
        .enumerate()
        .filter(|(_, d)| !matches!(d, LineDiff::Same(_)))
        .map(|(i, _)| i)
        .collect();
    let mut show = vec![false; diff.len()];
    for &c in &changed {
        let lo = c.saturating_sub(CONTEXT);
        let hi = (c + CONTEXT + 1).min(diff.len());
        for s in show.iter_mut().take(hi).skip(lo) {
            *s = true;
        }
    }

    let mut out = format!("{label}（{}）：\n", stat.describe());
    let mut rendered = 0usize;
    let mut skipped = 0usize;
    let mut last_shown: Option<usize> = None;
    for (i, d) in diff.iter().enumerate() {
        if !show[i] {
            skipped += 1;
            continue;
        }
        if rendered >= MAX_RENDERED {
            out.push_str(&format!(
                "  …（还有更多改动，只显示前 {MAX_RENDERED} 行）\n"
            ));
            break;
        }
        // 中间跳过了内容就说明一下，免得看起来像"上下行挨着"
        if skipped > 0 && last_shown.is_some() {
            out.push_str(&format!("  @@ 跳过 {skipped} 行未改动 @@\n"));
        }
        skipped = 0;
        last_shown = Some(i);
        match d {
            LineDiff::Same(s) => out.push_str(&format!("    {s}\n")),
            LineDiff::Removed(s) => out.push_str(&format!("  - {s}\n")),
            LineDiff::Added(s) => out.push_str(&format!("  + {s}\n")),
        }
        rendered += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(d: &[LineDiff]) -> Vec<char> {
        d.iter()
            .map(|x| match x {
                LineDiff::Same(_) => ' ',
                LineDiff::Added(_) => '+',
                LineDiff::Removed(_) => '-',
            })
            .collect()
    }

    // ---- 基本正确性 ----

    #[test]
    fn identical_text_has_no_changes() {
        let d = diff_lines("a\nb\nc", "a\nb\nc").unwrap();
        assert!(kinds(&d).iter().all(|c| *c == ' '));
        assert!(DiffStat::of(&d).is_empty());
    }

    #[test]
    fn an_added_line_shows_up_as_added() {
        let d = diff_lines("a\nc", "a\nb\nc").unwrap();
        let stat = DiffStat::of(&d);
        assert_eq!(stat.added, 1);
        assert_eq!(stat.removed, 0);
    }

    #[test]
    fn a_removed_line_shows_up_as_removed() {
        let d = diff_lines("a\nb\nc", "a\nc").unwrap();
        let stat = DiffStat::of(&d);
        assert_eq!(stat.added, 0);
        assert_eq!(stat.removed, 1);
    }

    #[test]
    fn a_changed_line_is_one_removal_and_one_addition() {
        let d = diff_lines("a\n旧\nc", "a\n新\nc").unwrap();
        let stat = DiffStat::of(&d);
        assert_eq!((stat.added, stat.removed), (1, 1));
    }

    #[test]
    fn the_lcs_keeps_unchanged_lines_unchanged() {
        // 不做 LCS 的话，中间插一行会被渲染成"后面全改了"——
        // 而那会让人以为改动比实际大得多
        let old = "1\n2\n3\n4\n5";
        let new = "1\n2\n新\n3\n4\n5";
        let d = diff_lines(old, new).unwrap();
        assert_eq!(DiffStat::of(&d).added, 1);
        assert_eq!(DiffStat::of(&d).removed, 0);
    }

    #[test]
    fn an_empty_old_file_means_everything_is_added() {
        let d = diff_lines("", "a\nb").unwrap();
        assert_eq!(DiffStat::of(&d).added, 2);
    }

    #[test]
    fn an_empty_new_file_means_everything_is_removed() {
        let d = diff_lines("a\nb", "").unwrap();
        assert_eq!(DiffStat::of(&d).removed, 2);
    }

    #[test]
    fn both_empty_is_no_change() {
        assert!(DiffStat::of(&diff_lines("", "").unwrap()).is_empty());
    }

    // ---- 确定性 ----

    #[test]
    fn the_same_input_gives_the_same_output() {
        // **审批提示每次长得不一样会让人以为改动也不一样。**
        let a = "x\ny\nz";
        let b = "x\nY\nz\nw";
        assert_eq!(diff_lines(a, b), diff_lines(a, b));
        assert_eq!(render(a, b, "t"), render(a, b, "t"));
    }

    // ---- 大小上限 ----

    #[test]
    fn a_huge_file_gives_up_instead_of_hanging() {
        // 几千行的 diff 没人读得完，而真正会被批准的是"看起来没问题的那个"
        let big = "行\n".repeat(MAX_LINES + 1);
        assert!(diff_lines(&big, "小").is_none());
    }

    #[test]
    fn giving_up_says_so_instead_of_pretending() {
        let big = "行\n".repeat(MAX_LINES + 1);
        let r = render(&big, "小", "要改 notes/a.md");
        assert!(r.contains("看不出"), "{r}");
        assert!(r.contains("自行确认"), "要说清接下来该怎么办: {r}");
    }

    #[test]
    fn a_file_at_the_limit_still_works() {
        let at_limit = "行\n".repeat(MAX_LINES);
        assert!(diff_lines(&at_limit, "小").is_some());
    }

    // ---- 渲染 ----

    #[test]
    fn rendering_marks_additions_and_removals() {
        let r = render("a\n旧\nc", "a\n新\nc", "要改 f.md");
        assert!(r.contains("- 旧"), "{r}");
        assert!(r.contains("+ 新"), "{r}");
    }

    #[test]
    fn rendering_says_what_it_is_about_to_do() {
        let r = render("a\n旧\nc", "a\n新\nc", "要改 notes/a.md");
        assert!(r.contains("notes/a.md"), "要写清动哪个文件: {r}");
        assert!(r.contains("加 1 行"), "{r}");
        assert!(r.contains("删 1 行"), "{r}");
    }

    #[test]
    fn rendering_keeps_only_the_neighbourhood_of_the_change() {
        // 一个 100 行的文件只改一行的话，人要在 100 行里找那一行
        let old: String = (1..=100).map(|i| format!("第 {i} 行\n")).collect();
        let new = old.replace("第 50 行", "第 50 行（改了）");
        let r = render(&old, &new, "要改 f.md");
        assert!(r.contains("第 50 行"), "改动本身要在: {r}");
        assert!(!r.contains("第 1 行"), "远处的行不该出现: {r}");
        // 上下文要在
        assert!(r.contains("第 47 行"), "改动前的上下文该在: {r}");
        assert!(r.contains("第 53 行"), "改动后的上下文该在: {r}");
    }

    #[test]
    fn rendering_says_when_it_skipped_lines() {
        // 不说明的话，上下两行看起来像是挨着的
        let old: String = (1..=100).map(|i| format!("第 {i} 行\n")).collect();
        let new = old
            .replace("第 20 行", "第 20 行 改")
            .replace("第 80 行", "第 80 行 改");
        let r = render(&old, &new, "f.md");
        assert!(r.contains("跳过"), "跳过的行要说出来: {r}");
    }

    #[test]
    fn no_change_is_said_plainly() {
        let r = render("a\nb", "a\nb", "要改 f.md");
        assert!(r.contains("没有变化"), "{r}");
    }

    #[test]
    fn rendering_is_bounded() {
        // 行数（不是字节数）要有上限
        let old: String = (1..=MAX_LINES).map(|i| format!("旧 {i}\n")).collect();
        let new: String = (1..=MAX_LINES).map(|i| format!("新 {i}\n")).collect();
        let r = render(&old, &new, "f.md");
        let lines = r.lines().count();
        assert!(lines <= MAX_RENDERED + 10, "渲染了 {lines} 行，太多了");
    }

    // ---- 统计措辞 ----

    #[test]
    fn a_pure_addition_reads_naturally() {
        let r = RenderProbe::stat(3, 0);
        assert_eq!(r, "加 3 行");
    }

    #[test]
    fn a_pure_removal_reads_naturally() {
        assert_eq!(RenderProbe::stat(0, 2), "删 2 行");
    }

    #[test]
    fn a_mixed_change_lists_both() {
        assert_eq!(RenderProbe::stat(2, 1), "加 2 行、删 1 行");
    }

    struct RenderProbe;
    impl RenderProbe {
        fn stat(added: usize, removed: usize) -> String {
            DiffStat { added, removed }.describe()
        }
    }
}
