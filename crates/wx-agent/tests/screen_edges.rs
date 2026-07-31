//! Which screen edges may take the cursor, from the layout down to the barriers.
//!
//! The three layers that answer this sit in three crates — the layout knows what
//! is beyond a screen, the agent knows where this machine's screens are in its
//! own desktop space, and the Wayland backend turns that into pointer barriers —
//! and no unit test of any one of them can catch the two disagreeing. This is the
//! reported bug at the level immediately below the compositor, which is as close
//! as it can be reproduced without arming a real capture session on somebody's
//! desktop.

use wx_agent::engine::local_exits;
use wx_core::layout::GlobalLayout;
use wx_platform::linux_wayland::capture::{barriers_for, BarrierEdge, Zone};
use wx_proto::{GlobalMonitorId, Monitor, MonitorId, NodeId, Placement, Rect};

fn node(n: u8) -> NodeId {
    NodeId([n; 32])
}

/// The reported arrangement, verbatim.
///
/// `cowen-ubuntu` at (6512, 5) 3072x1728 and `cowen-workhorse` at (3072, 144)
/// 3440x1440 — so the workhorse's right edge and the ubuntu machine's left edge
/// meet exactly at x=6512, and nothing lies above, below or to the right.
fn reported_layout() -> GlobalLayout {
    let mut l = GlobalLayout::new();
    l.insert(Placement {
        monitor: GlobalMonitorId::new(node(1), MonitorId(498_784_119)),
        global_bounds: Rect::new(6512, 5, 3072, 1728),
        cursor_scale: 1.0,
    });
    l.insert(Placement {
        monitor: GlobalMonitorId::new(node(2), MonitorId(515_561_738)),
        global_bounds: Rect::new(3072, 144, 3440, 1440),
        cursor_scale: 1.0,
    });
    l
}

/// The same screen as its own compositor sees it: at the origin, like any other.
fn ubuntu_screen() -> Monitor {
    Monitor {
        id: MonitorId(498_784_119),
        name: "DP-1".into(),
        local_bounds: Rect::new(0, 0, 3072, 1728),
        // The real machine: a 3840x2160 panel at scale 1.25, which the compositor
        // reports as a 3072x1728 logical screen at its own origin.
        scale: 1.25,
        primary: true,
    }
}

/// What the portal's `GetZones` reports for it, in the same space.
const UBUNTU_ZONE: Zone = Zone {
    x: 0,
    y: 0,
    w: 3072,
    h: 1728,
};

#[test]
fn only_the_edge_with_a_machine_beyond_it_can_take_the_cursor() {
    // The bug as reported: "when there is a screen attached to only 1 side, the
    // other 3 also try to grab the cursor". Three of these four edges must end up
    // with no barrier on them, or the compositor hands this agent the pointer at a
    // screen edge the user expects it to stop at — and the only way back is a
    // deliberate quarter-screen drag.
    let exits = local_exits(&reported_layout(), node(1), &[ubuntu_screen()]);
    let plans = barriers_for(&[UBUNTU_ZONE], &exits);

    assert_eq!(
        plans.iter().map(|p| p.edge).collect::<Vec<_>>(),
        vec![BarrierEdge::Left],
        "an edge with nothing beyond it armed a barrier"
    );
    // The crossing that should work still does, and still along the whole edge, so
    // the cursor keeps its height across the seam.
    assert_eq!(plans[0].position, (0, 0, 0, 1727));
}

#[test]
fn the_machine_on_the_other_side_arms_the_mirror_image() {
    // The same layout answered from the other end. Both machines must agree that
    // the one seam between them is the one live edge, or the cursor crosses and
    // cannot come back.
    let workhorse = Monitor {
        id: MonitorId(515_561_738),
        name: "DP-1".into(),
        local_bounds: Rect::new(0, 0, 3440, 1440),
        scale: 1.0,
        primary: true,
    };
    let zone = Zone {
        x: 0,
        y: 0,
        w: 3440,
        h: 1440,
    };
    let exits = local_exits(&reported_layout(), node(2), &[workhorse]);
    let plans = barriers_for(&[zone], &exits);

    assert_eq!(
        plans.iter().map(|p| p.edge).collect::<Vec<_>>(),
        vec![BarrierEdge::Right]
    );
    // The line sits on the boundary and its extent stops one short — the asymmetry
    // the compositor enforces and silently refuses otherwise.
    assert_eq!(plans[0].position, (3440, 0, 3440, 1439));
}

#[test]
fn a_machine_alone_in_the_layout_arms_nothing_at_all() {
    // Before any peer is paired, and after the last one is removed. The pointer
    // must stop on all four sides, exactly as it does with the agent not running.
    let mut alone = GlobalLayout::new();
    alone.insert(Placement {
        monitor: GlobalMonitorId::new(node(1), MonitorId(498_784_119)),
        global_bounds: Rect::new(0, 0, 3072, 1728),
        cursor_scale: 1.0,
    });
    let exits = local_exits(&alone, node(1), &[ubuntu_screen()]);
    assert!(barriers_for(&[UBUNTU_ZONE], &exits).is_empty());
}

#[test]
fn moving_the_peer_moves_which_edge_is_live() {
    // The layout editor is a drag-and-drop canvas, so this changes while the agent
    // runs. Each arrangement is answered from scratch — nothing is latched from the
    // one before — and the edge that was live must stop being live.
    let screen = ubuntu_screen();
    let here = GlobalMonitorId::new(node(1), MonitorId(498_784_119));
    let peer = GlobalMonitorId::new(node(2), MonitorId(515_561_738));

    // The peer above, below, right and left in turn.
    for (bounds, expected) in [
        (Rect::new(0, -1440, 3440, 1440), BarrierEdge::Top),
        (Rect::new(0, 1728, 3440, 1440), BarrierEdge::Bottom),
        (Rect::new(3072, 0, 3440, 1440), BarrierEdge::Right),
        (Rect::new(-3440, 0, 3440, 1440), BarrierEdge::Left),
    ] {
        let mut l = GlobalLayout::new();
        l.insert(Placement {
            monitor: here,
            global_bounds: Rect::new(0, 0, 3072, 1728),
            cursor_scale: 1.0,
        });
        l.insert(Placement {
            monitor: peer,
            global_bounds: bounds,
            cursor_scale: 1.0,
        });
        let exits = local_exits(&l, node(1), std::slice::from_ref(&screen));
        assert_eq!(
            barriers_for(&[UBUNTU_ZONE], &exits)
                .iter()
                .map(|p| p.edge)
                .collect::<Vec<_>>(),
            vec![expected],
            "peer at {bounds:?}"
        );
    }
}
