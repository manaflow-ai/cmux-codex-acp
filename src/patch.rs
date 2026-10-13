//! Unified diffs as ACP before/after text.
//!
//! Codex reports a file change as a unified diff per file. ACP's `diff`
//! content carries the whole file before (`oldText`) and after (`newText`),
//! so a client can draw it any way it likes. The file on disk is read once
//! when the change is announced and the hunks are applied to it here.

/// One hunk: the lines it expects (context and removed) and the lines it
/// leaves (context and added), at its 1-based old start line.
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    old: Vec<String>,
    new: Vec<String>,
    /// The old (or new) side ends without a newline at end of file.
    old_no_eol: bool,
    new_no_eol: bool,
}

fn parse(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    // The side the previous body line belonged to, for "\ No newline".
    let mut last: char = ' ';
    for line in diff.strip_suffix('\n').unwrap_or(diff).split('\n') {
        let header = if line == "@@" { Some("") } else { line.strip_prefix("@@ ") };
        if let Some(rest) = header {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            // "-a[,b] +c[,d] @@": a count is 1 when it is left out.
            let range = |sign: char| -> (usize, usize) {
                let part = rest.split(' ').find_map(|w| w.strip_prefix(sign)).unwrap_or("1");
                let mut it = part.split(',');
                let start = it.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                let count = it.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                (start, count)
            };
            let ((old_start, old_count), (new_start, new_count)) = (range('-'), range('+'));
            current = Some(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                old: Vec::new(),
                new: Vec::new(),
                old_no_eol: false,
                new_no_eol: false,
            });
            continue;
        }
        // File headers ("--- a", "+++ b") come before the first hunk only.
        let Some(h) = current.as_mut() else { continue };
        if line.starts_with('\\') {
            match last {
                '-' => h.old_no_eol = true,
                '+' => h.new_no_eol = true,
                _ => {
                    h.old_no_eol = true;
                    h.new_no_eol = true;
                }
            }
            continue;
        }
        let (tag, body) = match line.chars().next() {
            Some(c @ (' ' | '-' | '+')) => (c, &line[1..]),
            // A blank context line whose leading space was trimmed.
            None => (' ', ""),
            Some(_) => continue,
        };
        last = tag;
        match tag {
            '-' => h.old.push(body.to_owned()),
            '+' => h.new.push(body.to_owned()),
            _ => {
                h.old.push(body.to_owned());
                h.new.push(body.to_owned());
            }
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }
    hunks
}

fn lines(text: &str) -> (Vec<String>, bool) {
    let eol = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() && !eol {
        return (Vec::new(), false);
    }
    (body.split('\n').map(str::to_owned).collect(), eol)
}

fn find(haystack: &[String], needle: &[String], from: usize, hint: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(hint.clamp(from, haystack.len()));
    }
    let fits = |at: usize| at + needle.len() <= haystack.len() && haystack[at..at + needle.len()] == *needle;
    if hint >= from && fits(hint) {
        return Some(hint);
    }
    (from..haystack.len()).find(|&at| fits(at))
}

fn apply_hunks(text: &str, hunks: &[Hunk], forward: bool) -> Option<String> {
    let (src, mut eol) = lines(text);
    let mut out: Vec<String> = Vec::new();
    let mut at = 0;
    for h in hunks {
        let (want, put, want_no_eol, put_no_eol) = if forward {
            (&h.old, &h.new, h.old_no_eol, h.new_no_eol)
        } else {
            (&h.new, &h.old, h.new_no_eol, h.old_no_eol)
        };
        // A range of length 0 names the line after which the hunk goes.
        let (start, count) = if forward { (h.old_start, h.old_count) } else { (h.new_start, h.new_count) };
        let hint = if count == 0 { start } else { start.saturating_sub(1) };
        let pos = find(&src, want, at, hint)?;
        out.extend_from_slice(&src[at..pos]);
        out.extend(put.iter().cloned());
        at = pos + want.len();
        if at == src.len() && (want_no_eol || put_no_eol) {
            eol = !put_no_eol;
        }
    }
    out.extend_from_slice(&src[at..]);
    let mut s = out.join("\n");
    if eol && !out.is_empty() {
        s.push('\n');
    }
    Some(s)
}

/// The file after `diff`, applied to `old`. None when a hunk does not match.
pub fn apply(old: &str, diff: &str) -> Option<String> {
    let hunks = parse(diff);
    if hunks.is_empty() {
        return None;
    }
    apply_hunks(old, &hunks, true)
}

/// The file before `diff`, from the file after it (the change was already
/// written when the file was read).
pub fn unapply(new: &str, diff: &str) -> Option<String> {
    let hunks = parse(diff);
    if hunks.is_empty() {
        return None;
    }
    apply_hunks(new, &hunks, false)
}

/// A new file's text: its added lines, or the text itself when the change
/// carries the content rather than a diff.
pub fn added_text(diff: &str) -> String {
    if !is_diff(diff) {
        return diff.to_owned();
    }
    let hunks = parse(diff);
    if hunks.is_empty() {
        return diff.to_owned();
    }
    let mut s = hunks.iter().flat_map(|h| h.new.iter().cloned()).collect::<Vec<_>>().join("\n");
    if !hunks.last().is_some_and(|h| h.new_no_eol) {
        s.push('\n');
    }
    s
}

/// Before and after text of one file change. `old` is the file as read
/// when the change was announced (None when it could not be read).
pub fn before_after(kind: &str, old: Option<&str>, diff: &str) -> Option<(Option<String>, String)> {
    match kind {
        "add" => Some((None, added_text(diff))),
        "delete" => Some((Some(old.map(str::to_owned).unwrap_or_else(|| removed_text(diff))), String::new())),
        _ => {
            let old = old?;
            if let Some(new) = apply(old, diff) {
                return Some((Some(old.to_owned()), new));
            }
            // Already applied: the read file is the new text.
            let before = unapply(old, diff)?;
            Some((Some(before), old.to_owned()))
        }
    }
}

/// Whether a change carries a unified diff (else it is the file's text).
fn is_diff(text: &str) -> bool {
    text.starts_with("@@") || text.starts_with("--- ")
}

fn removed_text(diff: &str) -> String {
    if !is_diff(diff) {
        return diff.to_owned();
    }
    let hunks = parse(diff);
    let mut s = hunks.iter().flat_map(|h| h.old.iter().cloned()).collect::<Vec<_>>().join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}
