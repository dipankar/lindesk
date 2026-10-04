//! A small line-based unified diff (LCS), enough for config files.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Edit {
    Same(usize, usize),
    Del(usize),
    Ins(usize),
}

fn script(a: &[&str], b: &[&str]) -> Vec<Edit> {
    let (n, m) = (a.len(), b.len());
    // Trim the common prefix and suffix first: edits are usually small.
    let mut pre = 0;
    while pre < n && pre < m && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < n - pre && suf < m - pre && a[n - 1 - suf] == b[m - 1 - suf] {
        suf += 1;
    }
    let (ra, rb) = (&a[pre..n - suf], &b[pre..m - suf]);
    let mut out: Vec<Edit> = (0..pre).map(|i| Edit::Same(i, i)).collect();
    if ra.len().saturating_mul(rb.len()) > 4_000_000 {
        // Too big for the quadratic table: replace the middle wholesale.
        out.extend((0..ra.len()).map(|i| Edit::Del(pre + i)));
        out.extend((0..rb.len()).map(|j| Edit::Ins(pre + j)));
    } else {
        let (p, q) = (ra.len(), rb.len());
        let mut lcs = vec![vec![0u32; q + 1]; p + 1];
        for i in (0..p).rev() {
            for j in (0..q).rev() {
                lcs[i][j] = if ra[i] == rb[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < p || j < q {
            if i < p && j < q && ra[i] == rb[j] {
                out.push(Edit::Same(pre + i, pre + j));
                i += 1;
                j += 1;
            } else if i < p && (j == q || lcs[i + 1][j] >= lcs[i][j + 1]) {
                out.push(Edit::Del(pre + i));
                i += 1;
            } else {
                out.push(Edit::Ins(pre + j));
                j += 1;
            }
        }
    }
    out.extend((0..suf).map(|k| Edit::Same(n - suf + k, m - suf + k)));
    out
}

/// Unified diff of two texts with `context` lines of context. Empty when equal.
pub fn unified(a: &str, b: &str, a_label: &str, b_label: &str, context: usize) -> String {
    if a == b {
        return String::new();
    }
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let ed = script(&al, &bl);
    let changed: Vec<usize> =
        ed.iter().enumerate().filter(|(_, e)| !matches!(e, Edit::Same(..))).map(|(i, _)| i).collect();
    if changed.is_empty() {
        // Only trailing-newline differences.
        return format!("--- {a_label}\n+++ {b_label}\n@@ end of file @@\n\\ newline at end of file differs\n");
    }
    // Group changes into hunks.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &c in &changed {
        let start = c.saturating_sub(context);
        let end = (c + context + 1).min(ed.len());
        match hunks.last_mut() {
            Some(h) if start <= h.1 => h.1 = end,
            _ => hunks.push((start, end)),
        }
    }
    let mut out = format!("--- {a_label}\n+++ {b_label}\n");
    for (s, e) in hunks {
        let slice = &ed[s..e];
        // 1-based starting line numbers in each file.
        let a_start = ed[..s].iter().filter(|x| matches!(x, Edit::Same(..) | Edit::Del(_))).count();
        let b_start = ed[..s].iter().filter(|x| matches!(x, Edit::Same(..) | Edit::Ins(_))).count();
        let a_len = slice.iter().filter(|x| matches!(x, Edit::Same(..) | Edit::Del(_))).count();
        let b_len = slice.iter().filter(|x| matches!(x, Edit::Same(..) | Edit::Ins(_))).count();
        let num = |start: usize, len: usize| if len == 0 { start } else { start + 1 };
        out.push_str(&format!("@@ -{},{a_len} +{},{b_len} @@\n", num(a_start, a_len), num(b_start, b_len)));
        for x in slice {
            match *x {
                Edit::Same(i, _) => out.push_str(&format!(" {}\n", al[i])),
                Edit::Del(i) => out.push_str(&format!("-{}\n", al[i])),
                Edit::Ins(j) => out.push_str(&format!("+{}\n", bl[j])),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks() {
        let a = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let b = "a\nb\nc\nD\ne\nf\ng\nh\ni\nj\nk\n";
        let d = unified(a, b, "a/x", "b/x", 1);
        assert_eq!(d, "--- a/x\n+++ b/x\n@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n@@ -10,1 +10,2 @@\n j\n+k\n");
        assert_eq!(unified(a, a, "a", "b", 3), "");
        let d = unified("", "x = 1\n", "a", "b", 3);
        assert_eq!(d, "--- a\n+++ b\n@@ -0,0 +1,1 @@\n+x = 1\n");
    }
}
