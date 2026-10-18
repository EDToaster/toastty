//! Laid-out text and links (pixel space) → per-row [`TextRun`]s and
//! [`LinkSpan`]s (cell space).

use unicode_width::UnicodeWidthStr;

use crate::{LinkSpan, TextRun};

/// A piece of plain text with its pixel position.
#[derive(Debug, Clone)]
pub(crate) struct Frag {
    pub x: f32,
    pub baseline: f32,
    pub text: String,
}

/// The block row a baseline falls in.
pub(crate) fn row_of(baseline: f32, cell_h: u32) -> usize {
    ((baseline - 0.5).max(0.0) / cell_h as f32).floor() as usize
}

fn cells(s: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(s)).unwrap_or(u16::MAX)
}

/// Group `frags` by row and turn them into non-overlapping runs in
/// reading order. Frags in rows `>= rows` are dropped.
pub(crate) fn text_rows(
    frags: &[Frag],
    cell_w: u32,
    cell_h: u32,
    rows: usize,
) -> Vec<Vec<TextRun>> {
    let mut by_row: Vec<Vec<(usize, &Frag)>> = vec![vec![]; rows];
    for (i, f) in frags.iter().enumerate() {
        let r = row_of(f.baseline, cell_h);
        if r < rows && !f.text.is_empty() {
            by_row[r].push((i, f));
        }
    }
    by_row
        .into_iter()
        .map(|mut fs| {
            fs.sort_by(|a, b| a.1.x.total_cmp(&b.1.x).then(a.0.cmp(&b.0)));
            let mut runs: Vec<TextRun> = vec![];
            for (_, f) in fs {
                let desired = (f.x / cell_w as f32)
                    .round()
                    .clamp(0.0, f32::from(u16::MAX)) as u16;
                match runs.last_mut() {
                    Some(last) => {
                        let end = last.col.saturating_add(cells(&last.text));
                        let col = if desired < end {
                            end.saturating_add(u16::from(!last.text.ends_with(' ')))
                        } else {
                            desired
                        };
                        if col == end {
                            last.text.push_str(&f.text);
                        } else if col == end + 1 && desired < end {
                            last.text.push(' ');
                            last.text.push_str(&f.text);
                        } else {
                            runs.push(TextRun {
                                col,
                                text: f.text.clone(),
                            });
                        }
                    }
                    None => runs.push(TextRun {
                        col: desired,
                        text: f.text.clone(),
                    }),
                }
            }
            runs
        })
        .collect()
}

/// A link's pixel extent on a line.
#[derive(Debug, Clone)]
pub(crate) struct LinkPx {
    pub x0: f32,
    pub x1: f32,
    pub baseline: f32,
    pub url: String,
}

pub(crate) fn link_spans(
    links: &[LinkPx],
    cols: u16,
    cell_w: u32,
    cell_h: u32,
    rows: usize,
) -> Vec<LinkSpan> {
    let mut out: Vec<LinkSpan> = vec![];
    for l in links {
        let row = row_of(l.baseline, cell_h);
        if row >= rows {
            continue;
        }
        let c0 = (l.x0.max(0.0) / cell_w as f32).floor().min(f32::from(cols)) as u16;
        let c1 = (l.x1.max(0.0) / cell_w as f32).ceil().min(f32::from(cols)) as u16;
        if c1 <= c0 {
            continue;
        }
        let row = row as u16;
        if let Some(last) = out.last_mut()
            && last.row == row
            && last.url == l.url
            && last.cols.end >= c0
        {
            last.cols.end = last.cols.end.max(c1);
            continue;
        }
        out.push(LinkSpan {
            row,
            cols: c0..c1,
            url: l.url.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(x: f32, b: f32, t: &str) -> Frag {
        Frag {
            x,
            baseline: b,
            text: t.into(),
        }
    }

    #[test]
    fn rows_by_baseline_and_no_overlap() {
        let frags = [
            f(10.0, 17.0, "- "),
            f(30.0, 17.0, "item text"),
            f(60.0, 17.0, "more"),
            f(0.0, 39.0, "x"),
        ];
        let rows = text_rows(&frags, 10, 22, 3);
        assert_eq!(
            rows[0],
            vec![TextRun {
                col: 1,
                text: "- item text more".into()
            }]
        );
        assert_eq!(
            rows[1],
            vec![TextRun {
                col: 0,
                text: "x".into()
            }]
        );
        assert_eq!(rows[2], vec![]);
    }

    #[test]
    fn separate_runs_keep_columns() {
        let rows = text_rows(&[f(0.0, 10.0, "ab"), f(100.0, 10.0, "cd")], 10, 22, 1);
        assert_eq!(
            rows[0],
            vec![
                TextRun {
                    col: 0,
                    text: "ab".into()
                },
                TextRun {
                    col: 10,
                    text: "cd".into()
                }
            ]
        );
    }

    #[test]
    fn links_map_to_cells() {
        let l = [LinkPx {
            x0: 15.0,
            x1: 41.0,
            baseline: 30.0,
            url: "u".into(),
        }];
        let s = link_spans(&l, 80, 10, 22, 4);
        assert_eq!(
            s,
            vec![LinkSpan {
                row: 1,
                cols: 1..5,
                url: "u".into()
            }]
        );
    }
}
