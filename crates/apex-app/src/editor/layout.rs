//! Editor groups and their layout (pure, unit tested).
//!
//! The editor area is a tree of splits whose leaves are *groups*; each
//! group has its own tabs (file paths) and active tab, like Visual Studio's
//! and VS Code's editor groups. Slint cannot render recursive components,
//! so [`EditorLayout::rects`] flattens the tree into absolute rectangles for
//! the groups and the splitters between them.

use std::path::{Path, PathBuf};

/// Identifies a group for its lifetime.
pub type GroupId = u32;

/// Split direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Children side by side (left to right).
    Row,
    /// Children stacked (top to bottom).
    Column,
}

/// A node of the layout tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A group.
    Leaf(GroupId),
    /// A split; `sizes` are fractions that sum to 1.
    Split {
        /// Direction.
        dir: Dir,
        /// Children.
        children: Vec<Node>,
        /// Share of each child.
        sizes: Vec<f32>,
    },
}

/// One editor group.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// Its id.
    pub id: GroupId,
    /// Open files, in tab order.
    pub tabs: Vec<PathBuf>,
    /// Index of the active tab.
    pub active: Option<usize>,
}

impl Group {
    fn new(id: GroupId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: None,
        }
    }

    /// Path of the active tab.
    pub fn active_path(&self) -> Option<&Path> {
        self.active
            .and_then(|i| self.tabs.get(i))
            .map(PathBuf::as_path)
    }
}

/// Where a dragged tab is dropped relative to a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// Into the group.
    Center,
    /// A new group on the left.
    Left,
    /// A new group on the right.
    Right,
    /// A new group above.
    Top,
    /// A new group below.
    Bottom,
}

/// Rectangle of a group or splitter, in px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// A draggable splitter between two children of a split.
#[derive(Debug, Clone, PartialEq)]
pub struct Splitter {
    /// Path from the root to the split (child indices).
    pub path: Vec<usize>,
    /// The splitter sits after child `index`.
    pub index: usize,
    /// Where it is.
    pub rect: Rect,
    /// Drag horizontally (`Row` split) or vertically.
    pub dir: Dir,
    /// Size of the split in the drag direction (px), to convert deltas.
    pub extent: f32,
}

/// Thickness of a splitter in px.
pub const SPLITTER: f32 = 4.0;
/// Smallest share a group may be resized to.
const MIN_SHARE: f32 = 0.08;

/// The layout of editor groups.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorLayout {
    /// The tree.
    pub root: Node,
    /// Groups in creation order (pane index = position here).
    pub groups: Vec<Group>,
    /// The group that has focus (always one in the tree).
    pub focused: GroupId,
    /// Groups shown in their own window instead of the tree.
    pub floating: Vec<GroupId>,
    next_id: GroupId,
}

impl Default for EditorLayout {
    fn default() -> Self {
        Self {
            root: Node::Leaf(0),
            groups: vec![Group::new(0)],
            focused: 0,
            floating: Vec::new(),
            next_id: 1,
        }
    }
}

impl EditorLayout {
    /// Index of group `id` in [`groups`](Self::groups).
    pub fn index_of(&self, id: GroupId) -> Option<usize> {
        self.groups.iter().position(|g| g.id == id)
    }

    /// Group `id`.
    pub fn group(&self, id: GroupId) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    fn group_mut(&mut self, id: GroupId) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    /// The focused group.
    #[cfg(test)]
    pub fn focused_group(&self) -> &Group {
        self.group(self.focused).unwrap_or(&self.groups[0])
    }

    /// Opens `path` in the focused group (or activates it there).
    pub fn open(&mut self, path: &Path) {
        let focused = self.focused;
        if let Some(g) = self.group_mut(focused) {
            match g.tabs.iter().position(|t| t == path) {
                Some(i) => g.active = Some(i),
                None => {
                    let at = g.active.map_or(g.tabs.len(), |a| a + 1);
                    g.tabs.insert(at, path.to_path_buf());
                    g.active = Some(at);
                }
            }
        }
    }

    /// Activates tab `index` of group `id` and focuses the group.
    pub fn activate(&mut self, id: GroupId, index: usize) {
        if let Some(g) = self.group_mut(id) {
            if index < g.tabs.len() {
                g.active = Some(index);
            }
        }
        if self.group(id).is_some() {
            self.focused = id;
        }
    }

    /// Whether any group shows `path`.
    pub fn is_open(&self, path: &Path) -> bool {
        self.groups.iter().any(|g| g.tabs.iter().any(|t| t == path))
    }

    /// Removes `path` from group `id`; empty groups (except the last one)
    /// are closed. Returns whether no group shows the file any more.
    pub fn close(&mut self, id: GroupId, path: &Path) -> bool {
        if let Some(g) = self.group_mut(id) {
            if let Some(i) = g.tabs.iter().position(|t| t == path) {
                g.tabs.remove(i);
                g.active = match g.active {
                    _ if g.tabs.is_empty() => None,
                    Some(a) if a > i => Some(a - 1),
                    Some(a) if a == i => Some(i.min(g.tabs.len() - 1)),
                    other => other,
                };
            }
        }
        self.prune();
        !self.is_open(path)
    }

    /// Removes `path` from every group.
    pub fn close_everywhere(&mut self, path: &Path) {
        let ids: Vec<GroupId> = self.groups.iter().map(|g| g.id).collect();
        for id in ids {
            self.close(id, path);
        }
    }

    /// Moves tab `path` from group `from` to group `to` at `index` (clamped;
    /// `usize::MAX` appends). Moving within one group reorders.
    pub fn move_tab(&mut self, from: GroupId, path: &Path, to: GroupId, index: usize) {
        if self.group(to).is_none() {
            return;
        }
        let Some(src) = self.group_mut(from) else {
            return;
        };
        let Some(i) = src.tabs.iter().position(|t| t == path) else {
            return;
        };
        let tab = src.tabs.remove(i);
        let mut index = index;
        if from == to && index > i && index != usize::MAX {
            index -= 1;
        }
        if from != to {
            src.active = match src.active {
                _ if src.tabs.is_empty() => None,
                Some(a) if a >= src.tabs.len() => Some(src.tabs.len() - 1),
                Some(a) if a > i => Some(a - 1),
                other => other,
            };
        }
        if let Some(dst) = self.group_mut(to) {
            // A file is shown once per group.
            if let Some(dup) = dst.tabs.iter().position(|t| *t == tab) {
                dst.tabs.remove(dup);
            }
            let at = index.min(dst.tabs.len());
            dst.tabs.insert(at, tab);
            dst.active = Some(at);
        }
        self.focused = to;
        self.prune();
    }

    /// Splits group `id` in `zone` (not `Center`) and moves (or, with
    /// `copy`, opens) `path` into the new group. Returns the new group's id.
    pub fn split(
        &mut self,
        id: GroupId,
        zone: Zone,
        path: Option<&Path>,
        copy: bool,
    ) -> Option<GroupId> {
        let (dir, before) = match zone {
            Zone::Left => (Dir::Row, true),
            Zone::Right => (Dir::Row, false),
            Zone::Top => (Dir::Column, true),
            Zone::Bottom => (Dir::Column, false),
            Zone::Center => return None,
        };
        self.group(id)?;
        let new_id = self.next_id;
        self.next_id += 1;
        self.groups.push(Group::new(new_id));
        insert_split(&mut self.root, id, new_id, dir, before);
        if let Some(path) = path {
            if copy {
                if let Some(g) = self.group_mut(new_id) {
                    g.tabs.push(path.to_path_buf());
                    g.active = Some(0);
                }
                self.focused = new_id;
            } else {
                self.move_tab(id, path, new_id, 0);
            }
        }
        self.focused = new_id;
        self.prune();
        Some(new_id)
    }

    /// Moves `path` from group `from` into a new floating group (its own
    /// window). Returns the new group.
    pub fn detach(&mut self, from: GroupId, path: &Path) -> Option<GroupId> {
        if !self.group(from)?.tabs.iter().any(|t| t == path) {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.groups.push(Group::new(id));
        self.floating.push(id);
        let focused = self.focused;
        if let Some(src) = self.group_mut(from) {
            if let Some(i) = src.tabs.iter().position(|t| t == path) {
                src.tabs.remove(i);
                src.active = match src.active {
                    _ if src.tabs.is_empty() => None,
                    Some(a) if a >= src.tabs.len() => Some(src.tabs.len() - 1),
                    Some(a) if a > i => Some(a - 1),
                    other => other,
                };
            }
        }
        if let Some(g) = self.group_mut(id) {
            g.tabs.push(path.to_path_buf());
            g.active = Some(0);
        }
        self.focused = focused;
        self.prune();
        Some(id)
    }

    /// Whether group `id` lives in its own window.
    pub fn is_floating(&self, id: GroupId) -> bool {
        self.floating.contains(&id)
    }

    /// Moves every tab of floating group `id` back into the focused tree
    /// group and removes it.
    pub fn dock(&mut self, id: GroupId) {
        if !self.is_floating(id) {
            return;
        }
        let tabs = self.group(id).map(|g| g.tabs.clone()).unwrap_or_default();
        let target = self.focused;
        for t in tabs {
            self.move_tab(id, &t, target, usize::MAX);
        }
        self.groups.retain(|g| g.id != id);
        self.floating.retain(|f| *f != id);
        self.focused = target;
    }

    /// Splits group `id` in `zone` with a new, empty group (to drop a tab
    /// into right after). Returns the new group.
    pub fn split_empty(&mut self, id: GroupId, zone: Zone) -> Option<GroupId> {
        let (dir, before) = match zone {
            Zone::Left => (Dir::Row, true),
            Zone::Right => (Dir::Row, false),
            Zone::Top => (Dir::Column, true),
            Zone::Bottom => (Dir::Column, false),
            Zone::Center => return None,
        };
        if self.group(id).is_none() || self.is_floating(id) {
            return None;
        }
        let new_id = self.next_id;
        self.next_id += 1;
        self.groups.push(Group::new(new_id));
        insert_split(&mut self.root, id, new_id, dir, before);
        Some(new_id)
    }

    /// Closes empty groups (keeping at least one tree group) and collapses
    /// splits with a single child.
    fn prune(&mut self) {
        loop {
            let tree_groups = self.groups.len() - self.floating.len();
            let victim = self
                .groups
                .iter()
                .filter(|g| g.tabs.is_empty())
                .map(|g| g.id)
                .find(|id| self.floating.contains(id) || tree_groups > 1);
            let Some(victim) = victim else {
                break;
            };
            if self.floating.contains(&victim) {
                self.floating.retain(|f| *f != victim);
                self.groups.retain(|g| g.id != victim);
                continue;
            }
            self.groups.retain(|g| g.id != victim);
            remove_leaf(&mut self.root, victim);
            if self.focused == victim {
                self.focused = self
                    .groups
                    .iter()
                    .map(|g| g.id)
                    .find(|id| !self.floating.contains(id))
                    .unwrap_or(0);
            }
        }
        collapse(&mut self.root);
    }

    /// Group and splitter rectangles for an area of `w` x `h` px.
    pub fn rects(&self, w: f32, h: f32) -> (Vec<(GroupId, Rect)>, Vec<Splitter>) {
        let mut groups = Vec::new();
        let mut splitters = Vec::new();
        layout_node(
            &self.root,
            Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            &mut Vec::new(),
            &mut groups,
            &mut splitters,
        );
        (groups, splitters)
    }

    /// Focuses tree group `id` (floating groups never take the main focus).
    pub fn focus(&mut self, id: GroupId) {
        if self.group(id).is_some() && !self.is_floating(id) {
            self.focused = id;
        }
    }

    /// The group (tree groups first) that shows `path`.
    pub fn group_with(&self, path: &Path) -> Option<GroupId> {
        let shows = |g: &&Group| g.tabs.iter().any(|t| t == path);
        self.groups
            .iter()
            .filter(|g| !self.is_floating(g.id))
            .find(shows)
            .or_else(|| self.groups.iter().find(shows))
            .map(|g| g.id)
    }

    /// Drags splitter `index` of the split at `path` by `delta` px.
    pub fn resize(&mut self, path: &[usize], index: usize, delta: f32, extent: f32) {
        let Some(Node::Split { sizes, .. }) = node_at_mut(&mut self.root, path) else {
            return;
        };
        if index + 1 >= sizes.len() || extent <= 0.0 {
            return;
        }
        let d = delta / extent;
        let pair = sizes[index] + sizes[index + 1];
        let a = (sizes[index] + d).clamp(MIN_SHARE, pair - MIN_SHARE);
        sizes[index] = a;
        sizes[index + 1] = pair - a;
    }
}

/// The drop zone of a point inside `r`: the outer quarter on each side
/// splits, the middle drops into the group.
pub fn zone_at(r: Rect, x: f32, y: f32) -> Zone {
    let fx = (x - r.x) / r.w.max(1.0);
    let fy = (y - r.y) / r.h.max(1.0);
    let edges = [
        (fx, Zone::Left),
        (1.0 - fx, Zone::Right),
        (fy, Zone::Top),
        (1.0 - fy, Zone::Bottom),
    ];
    let (d, zone) = edges
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .unwrap_or((1.0, Zone::Center));
    if d < 0.25 {
        zone
    } else {
        Zone::Center
    }
}

/// The preview rectangle for dropping into `zone` of `r`.
pub fn zone_rect(r: Rect, zone: Zone) -> Rect {
    match zone {
        Zone::Center => r,
        Zone::Left => Rect { w: r.w / 2.0, ..r },
        Zone::Right => Rect {
            x: r.x + r.w / 2.0,
            w: r.w / 2.0,
            ..r
        },
        Zone::Top => Rect { h: r.h / 2.0, ..r },
        Zone::Bottom => Rect {
            y: r.y + r.h / 2.0,
            h: r.h / 2.0,
            ..r
        },
    }
}

fn insert_split(node: &mut Node, target: GroupId, new_id: GroupId, dir: Dir, before: bool) -> bool {
    match node {
        Node::Leaf(id) if *id == target => {
            let old = Node::Leaf(*id);
            let new = Node::Leaf(new_id);
            let children = if before {
                vec![new, old]
            } else {
                vec![old, new]
            };
            *node = Node::Split {
                dir,
                children,
                sizes: vec![0.5, 0.5],
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split {
            dir: d,
            children,
            sizes,
        } => {
            for i in 0..children.len() {
                if matches!(&children[i], Node::Leaf(id) if *id == target) && *d == dir {
                    // Same direction: add a sibling and share the space.
                    let at = if before { i } else { i + 1 };
                    let half = sizes[i] / 2.0;
                    sizes[i] = half;
                    children.insert(at, Node::Leaf(new_id));
                    sizes.insert(at, half);
                    return true;
                }
                if insert_split(&mut children[i], target, new_id, dir, before) {
                    return true;
                }
            }
            false
        }
    }
}

fn remove_leaf(node: &mut Node, target: GroupId) -> bool {
    let Node::Split {
        children, sizes, ..
    } = node
    else {
        return false;
    };
    if let Some(i) = children
        .iter()
        .position(|c| matches!(c, Node::Leaf(id) if *id == target))
    {
        children.remove(i);
        let freed = sizes.remove(i);
        let n = sizes.len().max(1) as f32;
        for s in sizes.iter_mut() {
            *s += freed / n;
        }
        return true;
    }
    children.iter_mut().any(|c| remove_leaf(c, target))
}

fn collapse(node: &mut Node) {
    if let Node::Split { children, .. } = node {
        for c in children.iter_mut() {
            collapse(c);
        }
        if children.len() == 1 {
            let only = children.remove(0);
            *node = only;
        }
    }
}

fn node_at_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    match path.split_first() {
        None => Some(node),
        Some((&i, rest)) => match node {
            Node::Split { children, .. } => node_at_mut(children.get_mut(i)?, rest),
            Node::Leaf(_) => None,
        },
    }
}

fn layout_node(
    node: &Node,
    r: Rect,
    path: &mut Vec<usize>,
    groups: &mut Vec<(GroupId, Rect)>,
    splitters: &mut Vec<Splitter>,
) {
    match node {
        Node::Leaf(id) => groups.push((*id, r)),
        Node::Split {
            dir,
            children,
            sizes,
        } => {
            let n = children.len();
            let total = match dir {
                Dir::Row => r.w,
                Dir::Column => r.h,
            };
            let usable = (total - SPLITTER * (n.saturating_sub(1)) as f32).max(0.0);
            let mut pos = 0.0;
            for (i, child) in children.iter().enumerate() {
                let len = if i + 1 == n {
                    usable - (pos - SPLITTER * i as f32)
                } else {
                    (usable * sizes[i]).round()
                };
                let cr = match dir {
                    Dir::Row => Rect {
                        x: r.x + pos,
                        w: len,
                        ..r
                    },
                    Dir::Column => Rect {
                        y: r.y + pos,
                        h: len,
                        ..r
                    },
                };
                path.push(i);
                layout_node(child, cr, path, groups, splitters);
                path.pop();
                pos += len;
                if i + 1 < n {
                    let sr = match dir {
                        Dir::Row => Rect {
                            x: r.x + pos,
                            w: SPLITTER,
                            ..r
                        },
                        Dir::Column => Rect {
                            y: r.y + pos,
                            h: SPLITTER,
                            ..r
                        },
                    };
                    splitters.push(Splitter {
                        path: path.clone(),
                        index: i,
                        rect: sr,
                        dir: *dir,
                        extent: usable,
                    });
                    pos += SPLITTER;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn open_activate_and_close() {
        let mut l = EditorLayout::default();
        l.open(&p("a.py"));
        l.open(&p("b.py"));
        assert_eq!(l.focused_group().tabs, vec![p("a.py"), p("b.py")]);
        assert_eq!(l.focused_group().active, Some(1));
        l.open(&p("a.py"));
        assert_eq!(l.focused_group().active, Some(0));
        assert!(l.close(0, &p("a.py")));
        assert_eq!(l.focused_group().active_path(), Some(Path::new("b.py")));
        assert!(l.close(0, &p("b.py")));
        assert_eq!(l.groups.len(), 1, "the last group stays");
    }

    #[test]
    fn split_right_then_down_builds_a_grid() {
        let mut l = EditorLayout::default();
        l.open(&p("a.py"));
        l.open(&p("b.py"));
        let right = l.split(0, Zone::Right, Some(&p("b.py")), false).unwrap();
        assert_eq!(l.focused, right);
        assert_eq!(l.group(0).unwrap().tabs, vec![p("a.py")]);
        assert_eq!(l.group(right).unwrap().tabs, vec![p("b.py")]);
        let below = l
            .split(right, Zone::Bottom, Some(&p("b.py")), true)
            .unwrap();
        assert!(
            l.group(right).unwrap().tabs.contains(&p("b.py")),
            "copy keeps the source"
        );
        let (rects, splitters) = l.rects(1000.0, 600.0);
        assert_eq!(rects.len(), 3);
        assert_eq!(splitters.len(), 2);
        let r0 = rects.iter().find(|(id, _)| *id == 0).unwrap().1;
        let rb = rects.iter().find(|(id, _)| *id == below).unwrap().1;
        assert_eq!(r0.h, 600.0);
        assert!(rb.x > 490.0 && rb.y > 290.0);
        // Widths add up with the splitter.
        let rr = rects.iter().find(|(id, _)| *id == right).unwrap().1;
        assert!((r0.w + SPLITTER + rr.w - 1000.0).abs() < 0.01);
    }

    #[test]
    fn same_direction_splits_share_one_row() {
        let mut l = EditorLayout::default();
        for f in ["a", "b", "c"] {
            l.open(&p(f));
        }
        let g1 = l.split(0, Zone::Right, Some(&p("b")), false).unwrap();
        l.split(g1, Zone::Right, Some(&p("b")), true).unwrap();
        match &l.root {
            Node::Split {
                children, sizes, ..
            } => {
                assert_eq!(children.len(), 3);
                assert!((sizes.iter().sum::<f32>() - 1.0).abs() < 1e-5);
            }
            Node::Leaf(_) => panic!("expected a split"),
        }
    }

    #[test]
    fn moving_the_last_tab_closes_the_group() {
        let mut l = EditorLayout::default();
        l.open(&p("a"));
        l.open(&p("b"));
        let g = l.split(0, Zone::Left, Some(&p("b")), false).unwrap();
        l.move_tab(g, &p("b"), 0, 0);
        assert_eq!(l.groups.len(), 1);
        assert_eq!(l.root, Node::Leaf(0));
        assert_eq!(l.group(0).unwrap().tabs, vec![p("b"), p("a")]);
        // Reorder within a group.
        l.move_tab(0, &p("b"), 0, usize::MAX);
        assert_eq!(l.group(0).unwrap().tabs, vec![p("a"), p("b")]);
        l.move_tab(0, &p("b"), 0, 0);
        assert_eq!(l.group(0).unwrap().tabs, vec![p("b"), p("a")]);
    }

    #[test]
    fn splitters_resize_with_limits() {
        let mut l = EditorLayout::default();
        l.open(&p("a"));
        l.open(&p("b"));
        l.split(0, Zone::Right, Some(&p("b")), false);
        let (_, s) = l.rects(1004.0, 500.0);
        l.resize(&s[0].path, s[0].index, 100.0, s[0].extent);
        let (r, _) = l.rects(1004.0, 500.0);
        assert!((r[0].1.w - 600.0).abs() < 1.0);
        l.resize(&s[0].path, s[0].index, -5000.0, s[0].extent);
        let (r, _) = l.rects(1004.0, 500.0);
        assert!(r[0].1.w >= 79.0);
    }

    #[test]
    fn detach_and_dock_windows() {
        let mut l = EditorLayout::default();
        l.open(&p("a"));
        l.open(&p("b"));
        let w = l.detach(0, &p("b")).unwrap();
        assert!(l.is_floating(w));
        assert_eq!(l.focused, 0);
        assert_eq!(
            l.rects(100.0, 100.0).0.len(),
            1,
            "floating groups are not in the tree"
        );
        assert_eq!(l.group_with(&p("b")), Some(w));
        l.dock(w);
        assert!(l.floating.is_empty());
        assert_eq!(l.group(0).unwrap().tabs, vec![p("a"), p("b")]);
        // Closing the last tab of a window removes its group.
        let w = l.detach(0, &p("a")).unwrap();
        assert!(l.close(w, &p("a")));
        assert!(l.group(w).is_none());
        assert_eq!(l.groups.len(), 1);
    }

    #[test]
    fn drop_zones() {
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 200.0,
        };
        assert_eq!(zone_at(r, 200.0, 100.0), Zone::Center);
        assert_eq!(zone_at(r, 20.0, 100.0), Zone::Left);
        assert_eq!(zone_at(r, 390.0, 100.0), Zone::Right);
        assert_eq!(zone_at(r, 200.0, 10.0), Zone::Top);
        assert_eq!(zone_at(r, 200.0, 190.0), Zone::Bottom);
        assert_eq!(zone_rect(r, Zone::Right).x, 200.0);
        assert_eq!(zone_rect(r, Zone::Bottom).h, 100.0);
    }
}
