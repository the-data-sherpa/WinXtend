//! Layout warnings, computed with the engine's own validator.
//!
//! The layout editor could check for overlaps in JavaScript in ten lines, and that
//! is exactly why this exists instead. Reachability is not a geometric property the
//! editor can guess at: it is whatever [`wx_core::GlobalLayout::resolve_crossing`]
//! decides, including its tie-breaking between two candidate screens and its rule
//! that a screen only counts as reachable if the trip is reversible. A second
//! implementation in the frontend would drift from the routing engine and then tell
//! the user their arrangement is fine while the cursor could never get there —
//! the single worst failure this screen can have, because it is silent.
//!
//! So the working layout is sent here on every edit and validated by the same code
//! the daemon will run.

use serde::{Deserialize, Serialize};
use wx_agent::ipc::LayoutSpec;
use wx_core::{GlobalLayout, LayoutProblem};

/// One monitor, named the way the wire and the UI both name them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRef {
    /// Owning node, hex encoded.
    pub node: String,
    pub monitor: u32,
}

impl MonitorRef {
    fn of(id: wx_proto::GlobalMonitorId) -> Self {
        Self {
            node: id.node.to_hex(),
            monitor: id.monitor.0,
        }
    }
}

/// Which kind of problem, as a string the frontend switches on.
///
/// Mirrors [`LayoutProblem`] but without its payload shape, because the frontend
/// wants a flat list of monitors to highlight and does not care that an overlap
/// names two and a degenerate rectangle names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProblemKind {
    Overlap,
    Unreachable,
    Degenerate,
}

/// A problem to draw on the canvas.
///
/// The wording lives in the frontend, which is the only side that knows the
/// monitor's label and its machine's user-chosen name; sending English from here
/// would mean either duplicating that lookup or showing hex node ids in a warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemView {
    pub kind: ProblemKind,
    /// Monitors the problem is about, in the order the engine reported them.
    pub refs: Vec<MonitorRef>,
}

/// Validate a working layout that has not been saved yet.
pub fn validate(layout: &LayoutSpec) -> Vec<ProblemView> {
    GlobalLayout::from_layout(&layout.to_layout())
        .validate()
        .into_iter()
        .map(|problem| match problem {
            LayoutProblem::Overlap { a, b } => ProblemView {
                kind: ProblemKind::Overlap,
                refs: vec![MonitorRef::of(a), MonitorRef::of(b)],
            },
            LayoutProblem::Unreachable(id) => ProblemView {
                kind: ProblemKind::Unreachable,
                refs: vec![MonitorRef::of(id)],
            },
            LayoutProblem::Degenerate(id) => ProblemView {
                kind: ProblemKind::Degenerate,
                refs: vec![MonitorRef::of(id)],
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_agent::ipc::PlacementSpec;
    use wx_proto::NodeId;

    fn node(n: u8) -> String {
        NodeId([n; 32]).to_hex()
    }

    fn screen(n: u8, monitor: u32, x: i32, y: i32, w: u32, h: u32) -> PlacementSpec {
        PlacementSpec {
            node: node(n),
            monitor,
            x,
            y,
            w,
            h,
            cursor_scale: 1.0,
        }
    }

    fn layout(placements: Vec<PlacementSpec>) -> LayoutSpec {
        LayoutSpec {
            revision: 1,
            placements,
        }
    }

    #[test]
    fn two_screens_side_by_side_are_reported_as_healthy() {
        let problems = validate(&layout(vec![
            screen(1, 0, 0, 0, 1920, 1080),
            screen(2, 0, 1920, 0, 1920, 1080),
        ]));
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn an_overlap_names_both_screens_so_both_can_be_highlighted() {
        let problems = validate(&layout(vec![
            screen(1, 0, 0, 0, 1920, 1080),
            screen(2, 3, 1000, 0, 1920, 1080),
        ]));
        let overlap = problems
            .iter()
            .find(|p| p.kind == ProblemKind::Overlap)
            .expect("an overlap");
        assert_eq!(
            overlap.refs,
            vec![
                MonitorRef {
                    node: node(1),
                    monitor: 0
                },
                MonitorRef {
                    node: node(2),
                    monitor: 3
                },
            ]
        );
    }

    #[test]
    fn a_screen_dropped_inside_another_is_reported_as_unreachable() {
        // The mistake this catches: a laptop panel dragged on top of a 4K screen
        // instead of beside it. Nothing is beyond any of its four edges, so the
        // engine can never resolve a crossing onto it and the cursor can never get
        // there — which is invisible on the canvas without the warning, because the
        // rectangle is sitting right there in plain view.
        let problems = validate(&layout(vec![
            screen(1, 0, 0, 0, 3840, 2160),
            screen(2, 0, 500, 500, 1280, 800),
        ]));
        assert!(
            problems.iter().any(|p| p.kind == ProblemKind::Overlap),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.kind == ProblemKind::Unreachable
                && p.refs
                    == vec![MonitorRef {
                        node: node(2),
                        monitor: 0
                    }]),
            "{problems:?}"
        );
    }

    #[test]
    fn a_gap_between_two_screens_is_not_a_warning() {
        // The routing engine crosses gaps rather than blocking on them, so an
        // arrangement with space between two screens still works. Warning about it
        // would train the user to ignore the warnings that matter.
        let problems = validate(&layout(vec![
            screen(1, 0, 0, 0, 1920, 1080),
            screen(2, 0, 6000, 400, 1280, 800),
        ]));
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_zero_size_rectangle_is_reported_rather_than_silently_accepted() {
        // Reachable by nothing and able to hold no cursor: dragging a monitor to
        // zero width in the editor must not look like a valid arrangement.
        let problems = validate(&layout(vec![
            screen(1, 0, 0, 0, 1920, 1080),
            screen(2, 0, 1920, 0, 0, 1080),
        ]));
        assert!(
            problems
                .iter()
                .any(|p| p.kind == ProblemKind::Degenerate
                    && p.refs[0].node == node(2)),
            "{problems:?}"
        );
    }

    #[test]
    fn the_object_the_editor_sends_over_the_bridge_deserialises_as_a_layout() {
        // Pins the field names the frontend has to use. A layout is the one type
        // shared with the config file, so its fields are snake_case while the rest of
        // the IPC surface is camelCase — the kind of asymmetry that is discovered at
        // runtime as a cursor speed that silently resets to 1.0.
        let text = format!(
            r#"{{"revision":4,"placements":[{{"node":"{}","monitor":2,"x":-1920,"y":40,"w":1920,"h":1080,"cursor_scale":1.25}}]}}"#,
            node(7)
        );
        let layout: LayoutSpec = serde_json::from_str(&text).unwrap();
        assert_eq!(layout.revision, 4);
        assert_eq!(layout.placements[0].cursor_scale, 1.25);
        assert_eq!(layout.placements[0].x, -1920);
        assert!(validate(&layout).is_empty());

        // The camelCase spelling is not an error; it is silently ignored and the
        // default is used. That is exactly why the test above exists.
        let camel = format!(
            r#"{{"revision":1,"placements":[{{"node":"{}","monitor":0,"x":0,"y":0,"w":1,"h":1,"cursorScale":2.0}}]}}"#,
            node(7)
        );
        let layout: LayoutSpec = serde_json::from_str(&camel).unwrap();
        assert_eq!(layout.placements[0].cursor_scale, 1.0);
    }

    #[test]
    fn an_empty_layout_has_nothing_to_warn_about() {
        assert!(validate(&LayoutSpec::default()).is_empty());
    }

    #[test]
    fn a_single_screen_is_not_called_unreachable() {
        // It is where the cursor already is; warning about it would make a
        // fresh single-machine install look broken.
        assert!(validate(&layout(vec![screen(1, 0, 0, 0, 2560, 1440)])).is_empty());
    }

    #[test]
    fn a_placement_with_an_unreadable_node_id_is_dropped_not_fatal() {
        // The frontend builds this list from whatever it has on screen; a bad hex
        // string arriving from an older config must not take the whole editor down.
        let problems = validate(&layout(vec![
            PlacementSpec {
                node: "not-hex".into(),
                monitor: 0,
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                cursor_scale: 1.0,
            },
            screen(2, 0, 0, 0, 1920, 1080),
        ]));
        // One screen survives, and a lone screen is never a problem.
        assert!(problems.is_empty(), "{problems:?}");
    }
}
