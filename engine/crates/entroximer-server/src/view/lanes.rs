//! Lays out a graph as rows to draw from top to bottom, the way `git log --graph` draws history:
//! one node a row, after every node feeding it, with each wire running down its own column (a
//! lane) of a gutter until it reaches the node it feeds. Where a node feeds several, its wire
//! forks into new lanes to its right; where several feed one, their lanes join into the leftmost.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

pub(super) struct Row {
    pub(super) gutter: String,
    /// The node drawn on this row; `None` on rows where wires only fork or join.
    pub(super) node: Option<usize>,
}

pub(super) struct Layout {
    pub(super) rows: Vec<Row>,
    /// The most lanes side by side; every gutter is `2 * lanes - 1` characters wide.
    pub(super) lanes: usize,
}

/// Lays out nodes drawn as `glyphs`, connected by `edges` (`(from, to)`, which must not form a
/// cycle). Of the nodes ready to be drawn, the one listed first comes first, so a chain listed
/// in order stays together, and the graph reads in the order it was written.
pub(super) fn lay_out(glyphs: &[char], edges: &[(usize, usize)]) -> Layout {
    let mut edges = edges.to_vec();
    edges.sort_unstable();
    edges.dedup();
    let mut successors = vec![Vec::new(); glyphs.len()];
    let mut feeding = vec![0usize; glyphs.len()];
    for &(from, to) in &edges {
        successors[from].push(to);
        feeding[to] += 1;
    }
    let mut ready: BinaryHeap<Reverse<usize>> = (0..glyphs.len())
        .filter(|&node| feeding[node] == 0)
        .map(Reverse)
        .collect();

    // the node each lane's wire runs to; `None` for a free lane
    let mut lanes: Vec<Option<usize>> = Vec::new();
    let mut rows = Vec::with_capacity(glyphs.len());
    let mut widest = 0;
    while let Some(Reverse(node)) = ready.pop() {
        let arriving: Vec<usize> = (0..lanes.len())
            .filter(|&lane| lanes[lane] == Some(node))
            .collect();
        let lane = match arriving.split_first() {
            Some((&first, rest)) => {
                if !rest.is_empty() {
                    rows.push(Row {
                        gutter: junction(&lanes, first, rest, Junction::Join),
                        node: None,
                    });
                    for &joined in rest {
                        lanes[joined] = None;
                    }
                }
                first
            }
            // a node nothing feeds starts a wire of its own
            None => free_lane(&mut lanes, 0),
        };
        rows.push(Row {
            gutter: node_row(&lanes, lane, glyphs[node]),
            node: Some(node),
        });

        let feeds = &successors[node];
        lanes[lane] = feeds.first().copied();
        if let Some(more) = feeds.get(1..).filter(|more| !more.is_empty()) {
            let forks: Vec<usize> = more
                .iter()
                .map(|&next| {
                    let fork = free_lane(&mut lanes, lane + 1);
                    lanes[fork] = Some(next);
                    fork
                })
                .collect();
            rows.push(Row {
                gutter: junction(&lanes, lane, &forks, Junction::Fork),
                node: None,
            });
        }
        widest = widest.max(lanes.len());
        for &next in feeds {
            feeding[next] -= 1;
            if feeding[next] == 0 {
                ready.push(Reverse(next));
            }
        }
        while lanes.last() == Some(&None) {
            lanes.pop();
        }
    }

    let width = (2 * widest).saturating_sub(1);
    for row in &mut rows {
        let drawn = row.gutter.chars().count();
        row.gutter
            .extend(std::iter::repeat_n(' ', width.saturating_sub(drawn)));
    }
    Layout {
        rows,
        lanes: widest,
    }
}

/// The first free lane from `from` on, added if there is none.
fn free_lane(lanes: &mut Vec<Option<usize>>, from: usize) -> usize {
    match (from..lanes.len()).find(|&lane| lanes[lane].is_none()) {
        Some(lane) => lane,
        None => {
            lanes.resize(lanes.len().max(from) + 1, None);
            lanes.len() - 1
        }
    }
}

/// The node on `lane`, and the wires running past it.
fn node_row(lanes: &[Option<usize>], lane: usize, glyph: char) -> String {
    draw(lanes.len(), |column| {
        let cell = if column == lane {
            glyph
        } else if lanes[column].is_some() {
            '│'
        } else {
            ' '
        };
        (cell, ' ')
    })
}

#[derive(Clone, Copy)]
enum Junction {
    Fork,
    Join,
}

/// The wire on lane `at` forking into, or joined by, the wires on `others`, which lie to its
/// right; wires in between cross.
fn junction(lanes: &[Option<usize>], at: usize, others: &[usize], kind: Junction) -> String {
    let far = others.iter().copied().max().unwrap_or(at);
    let (end, between) = match kind {
        Junction::Fork => ('┐', '┬'),
        Junction::Join => ('┘', '┴'),
    };
    draw(lanes.len(), |column| {
        let cell = if column == at {
            '├'
        } else if column == far {
            end
        } else if others.contains(&column) {
            between
        } else if (at..far).contains(&column) {
            if lanes[column].is_some() {
                '┼'
            } else {
                '─'
            }
        } else if lanes[column].is_some() {
            '│'
        } else {
            ' '
        };
        let after = if (at..far).contains(&column) {
            '─'
        } else {
            ' '
        };
        (cell, after)
    })
}

/// A gutter of `lanes` columns, from each column's cell and what follows it before the next.
fn draw(lanes: usize, mut column: impl FnMut(usize) -> (char, char)) -> String {
    let mut gutter = String::new();
    for lane in 0..lanes {
        let (cell, after) = column(lane);
        gutter.push(cell);
        if lane + 1 < lanes {
            gutter.push(after);
        }
    }
    gutter
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout with each row's gutter and, for a node, its name.
    fn drawn(names: &[&str], edges: &[(usize, usize)]) -> Vec<String> {
        let layout = lay_out(&vec!['●'; names.len()], edges);
        layout
            .rows
            .iter()
            .map(|row| {
                let name = row.node.map_or("", |node| names[node]);
                format!("{} {name}", row.gutter).trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn a_chain_is_one_lane() {
        let layout = lay_out(&['■', '●', '□'], &[(0, 1), (1, 2)]);
        assert_eq!(layout.lanes, 1);
        let gutters: Vec<_> = layout.rows.iter().map(|row| row.gutter.as_str()).collect();
        assert_eq!(gutters, ["■", "●", "□"]);
    }

    #[test]
    fn branches_fork_and_join() {
        // karaoke: the input split into a low and a high band, the high band's vocals cancelled
        let rows = drawn(
            &["input", "low", "high", "cancel", "output"],
            &[(0, 1), (0, 2), (2, 3), (1, 4), (3, 4)],
        );
        assert_eq!(
            rows,
            [
                "●   input",
                "├─┐",
                "● │ low",
                "│ ● high",
                "│ ● cancel",
                "├─┘",
                "●   output",
            ]
        );
    }

    #[test]
    fn sources_start_lanes_and_join_at_the_end() {
        let rows = drawn(
            &["music", "volume", "sfx", "volume", "voice", "volume", "mix"],
            &[(0, 1), (1, 6), (2, 3), (3, 6), (4, 5), (5, 6)],
        );
        assert_eq!(
            rows,
            [
                "●     music",
                "●     volume",
                "│ ●   sfx",
                "│ ●   volume",
                "│ │ ● voice",
                "│ │ ● volume",
                "├─┴─┘",
                "●     mix",
            ]
        );
    }

    #[test]
    fn wires_cross_those_in_between() {
        // `a` feeds `d` past `b`'s wire to `c`
        let rows = drawn(&["b", "a", "c", "d"], &[(0, 2), (1, 2), (1, 3), (2, 3)]);
        assert_eq!(
            rows,
            [
                "●     b",
                "│ ●   a",
                "│ ├─┐",
                "├─┘ │",
                "●   │ c",
                "├───┘",
                "●     d",
            ]
        );
    }

    #[test]
    fn duplicate_edges_draw_one_wire() {
        // like `a:0 -> b:0` and `a:1 -> b:1`
        let rows = drawn(&["a", "b"], &[(0, 1), (0, 1)]);
        assert_eq!(rows, ["● a", "● b"]);
    }

    #[test]
    fn every_gutter_is_as_wide_as_the_widest() {
        let layout = lay_out(&['●'; 4], &[(0, 1), (0, 2), (0, 3)]);
        assert_eq!(layout.lanes, 3);
        assert!(
            layout
                .rows
                .iter()
                .all(|row| row.gutter.chars().count() == 5)
        );
        assert_eq!(layout.rows[1].gutter, "├─┬─┐");
    }
}
