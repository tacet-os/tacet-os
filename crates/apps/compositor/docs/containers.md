# Containers, focus paths, and group fullscreen

Status: design, no code yet. Greenlights a follow-up PR series; once any
of these structures are committed, this doc should be deleted or moved
inline to the structs it describes — design docs rot, doc-comments don't.

## Goal

Extend the free-overlap canvas model with a single new primitive — the
**container** — that gives the user:

1. App grouping with a tab strip at the group's top-left corner.
2. Optional internal tiling inside a group (so a group can hold a small
   split layout).
3. A focus *path* (not a single focused window) so highlight rendering,
   spawn routing, and arrow-key nav all read from the same source.
4. Spatial focus navigation between visible windows with `Super+arrows`.
5. Transient "fullscreen this container" for focus mode.

Free-overlap drag-and-reflow ([[feedback_smooth_collision_resolve]]) is
unchanged. Groups *are* draggable surfaces themselves and reflow on
release exactly like single windows.

## Non-goals

- Workspaces / virtual desktops. The infinite canvas + zoom is already
  the navigation model — adding workspaces on top would duplicate it.
- A global tiling tree (i3/sway-style). Tiling is local to one group.
- Configurable spawn-mode state. Spawn routing reads the focus path and
  the launch-time modifier — no sticky modes.

## Current state of the world (June 2026)

- `Space<WindowElement>` in `state.rs` holds a flat list of windows.
- `WindowElement(pub Window)` in `shell/element.rs` wraps smithay's
  `desktop::Window`. SSD header bar already lives here.
- `KeyboardFocusTarget` in `focus.rs` is `Window | LayerSurface | Popup`
  — leaf-only.
- `Camera` in `camera.rs` is the existing logical→presentation transform
  (pan + zoom with target/current + exponential decay).
- `WindowAnimator` in `window_animator.rs` is the existing per-window
  slide animator with the same exponential-decay shape as `Camera`.
- `place_new_window` in `shell/mod.rs:394` picks a free slot near the
  pointer; `WindowAnimator::start` glides the window there.
- `KeyAction` in `input_handler.rs:1635` is the flat action enum;
  `process_keyboard_shortcut` builds one from a modifier+keysym.

## Data model

### Container tree

`Space<WindowElement>` becomes `Space<Container>`, where:

```rust
pub enum Container {
    Leaf(WindowElement),
    Group(Group),
}

pub struct Group {
    pub id: GroupId,
    pub children: Vec<Container>,
    pub active: usize,           // index into children; the "visible" tab
    pub layout: GroupLayout,
    pub decoration: GroupDecoration, // tab strip + frame
}

pub enum GroupLayout {
    /// Only `children[active]` is visible. Tab strip shows all.
    Tabbed,
    /// All children visible, arranged by `tiling`.
    Tiled { tiling: TilingSpec },
}
```

The recursion (`Group` contains `Container`, not `Leaf`) means tabs can
themselves be groups. We do not advertise this in the UI initially —
single-level groups cover the user's stated need — but the data model
shouldn't preclude it, and it's the path of least resistance for "tile
inside a group" since a tiled group is just one tile slot per child.

### Focus path

`KeyboardFocusTarget` stays leaf-only — there's no `Group` variant.
Wayland clients only ever see "I am focused" or not; from a protocol
standpoint there's no such thing as group focus. The compositor instead
maintains:

```rust
pub struct FocusPath {
    /// From root container down to the focused leaf. Last element is
    /// always a `Leaf`. Empty iff nothing is focused.
    pub nodes: Vec<NodeId>,
}
```

Each `NodeId` identifies a position in the container tree. The focused
leaf is `nodes.last()`; ancestor groups are `nodes[..len-1]`.

Why a path and not a single pointer:

- **Highlight rendering** needs to know "this leaf is focused" *and*
  "this group has a focused descendant" — different chrome states. A
  path gives both in one read.
- **Spawn routing** asks "which container receives the new child?" —
  that's a specific node on the path, picked by the launch modifier
  (innermost group for tab/tile, root for floating).
- **Arrow nav at group level** (`Super+Shift+arrows`) navigates between
  containers at `nodes[len-2]`'s level. Same algorithm as leaf-level,
  different node.

### Logical vs presentation geometry

`Camera` already provides this split for pan/zoom: the canvas position
of every window is logical; what gets composited is `logical ·
camera.transform`. Containers extend it with one more layer:

```rust
pub struct Container {
    pub logical_rect: Rectangle<i32, Logical>,
    pub presentation: Presentation,
    // …
}

pub enum Presentation {
    /// Composite at `logical_rect · camera`.
    Normal,
    /// Composite at the output's viewport rect, ignoring camera.
    /// `from` lets the animator interpolate from logical → viewport.
    Fullscreen { output: OutputId, from: Rectangle<i32, Logical> },
}
```

Fullscreen is a per-container flag, not a global mode. A single
container being fullscreen on one output doesn't affect others. The
logical rect stays correct underneath, so toggling fullscreen off
animates back to exactly where the container lived in the canvas.

## Behavior

### Spawn routing

`place_new_window` gains a `mode: SpawnMode` parameter:

```rust
pub enum SpawnMode {
    /// Append as a tab to the innermost focused group. If the focus
    /// path has no group (only a Leaf at root), promote that Leaf into
    /// a new tabbed group with the existing window as tab[0] and the
    /// new window as tab[1].
    AsTab,
    /// Add to the innermost focused group's tiled layout. If the group
    /// is Tabbed, switch it to Tiled with the existing children
    /// becoming the first tile. If no group on path, same promotion as
    /// AsTab but the result is Tiled.
    AsTile,
    /// Existing behavior: free-floating window placed by free-slot
    /// search near the pointer.
    Floating,
}
```

The `AsTab` / `AsTile` promotion path is the bit that makes this
feature usable without prior setup: the user doesn't have to explicitly
"create a group" — focusing a window and pressing `Super+Enter`
implicitly turns it into a group with the new app joining as tab 2.

### Fullscreen

`Super+F` (or whatever the binding settles on — see `KeyAction` below)
toggles `Presentation::Fullscreen` on the topmost group on the focus
path, or on the focused leaf if no group is on the path.

A new animator alongside `WindowAnimator` interpolates `from` →
`output.viewport_rect` over the same τ=80 ms exponential decay used by
`Camera`. The `is_animating()` gate joins the OR-chain in the render
loop's "any animator still going?" check.

Crucially: while fullscreen, the inner tab strip and internal-tiling
chrome render *at viewport scale*, not at canvas zoom. That's why
fullscreen needs to ignore `Camera`, not just override `logical_rect`.

Per [[project_object_os_zoom_state_replication]]: `Presentation`
affects what `xdg_toplevel.configure` size goes to the client, and the
fullscreen flag in the configure. That state has to be applied in both
the toggle event path *and* the per-frame `post_repaint`, or anvil's
re-emit will fight the toggle and the client will flicker between
windowed and fullscreen sizes.

### Spatial navigation

`focus_dir(direction: Direction, level: NavLevel)` is the single
primitive:

- `level = Leaf`: candidates are visible leaf rectangles. The focused
  leaf is excluded.
- `level = Group`: candidates are the rectangles of containers at
  `focus_path.nodes[len-2]`'s sibling positions (i.e., one level up).

Cone scoring: from the focus rectangle's center, project a cone of
±45° in `direction`. Each candidate's center is scored
`angular_offset.abs() * ANGLE_WEIGHT + distance`, lowest wins.
`ANGLE_WEIGHT` tuned so that "the obvious neighbor 5px diagonal" beats
"the further-away neighbor exactly on-axis" — i3 historically got this
wrong and it felt awful.

After the focused leaf changes, the camera animates to bring it on
screen if it's outside the viewport. Reuses `Camera::zoom_around` style
math: solve for the camera position that puts the new focused window's
center at the viewport center; let the existing animator step there.

Within-group tab cycling (`focus_next_tab` / `focus_prev_tab`) is
separate from spatial nav — it changes `Group::active`, doesn't move
the focus path's structure, and doesn't move the camera.

### Key bindings (provisional)

Action vocabulary first, keys are a thin config layer over the top. New
`KeyAction` variants:

```rust
SpawnAsTab(Vec<String>),     // Super+Enter <argv>
SpawnAsTile(Vec<String>),    // Super+Shift+Enter <argv>
FullscreenContainer,         // Super+F
FocusDir(Direction, NavLevel), // Super+Arrow, Super+Shift+Arrow
FocusNextTab, FocusPrevTab,  // Super+Tab, Super+Shift+Tab (TBD)
```

`SpawnAsTab`/`SpawnAsTile` reuse the existing `Run` machinery for the
process side; only the placement diverges.

## Touch points by file

| File | Change |
| --- | --- |
| `shell/element.rs` | `WindowElement` becomes a leaf variant of `Container`. SSD header logic stays on the leaf. |
| `shell/mod.rs` | `place_new_window` gains `SpawnMode`. Promotion-to-group logic lives here. |
| `shell/xdg.rs` | `new_toplevel` reads the seat's pending `SpawnMode` (set by the input handler that fired the launch) and passes it through. Pending-mode lifetime: cleared on first window of the launch, or on a 2s timeout to avoid bleeding into the next manual launch. |
| `focus.rs` | New `FocusPath` struct lives alongside `KeyboardFocusTarget`. `From<FocusPath> for KeyboardFocusTarget` extracts the leaf for Wayland's seat. |
| `input_handler.rs` | New `KeyAction` variants; `process_common_key_action` dispatch for each. Spatial nav cone-scoring lives here or in a new `nav.rs`. |
| `state.rs` | `Space<WindowElement>` → `Space<Container>`. `FocusPath` joins the state struct. `pending_spawn_mode: Option<(SpawnMode, Instant)>`. |
| `window_animator.rs` | Either extend with `ContainerAnim` for group geometry + fullscreen, or split into a sibling animator. The pattern (exponential decay, `is_animating()`, snap eps) is reused verbatim. |
| `render.rs` | Group decoration (tab strip + frame) becomes a render-element source. Highlight rendering reads `FocusPath` to decide leaf border + ancestor-group chrome. Fullscreen path skips `RescaleRenderElement` for the affected container. |
| `camera.rs` | Unchanged. Fullscreen explicitly bypasses it. |
| `grid.rs` | Unchanged. |

## Parallel-safety with in-flight work

The "two agents working on terminal, browser and view" branches don't
appear in `~/Developer/tacet-os` as worktrees or sibling clones — they
must be on separate machines. Risks if their work overlaps:

- **Compositor focus/input routing**: only collision risk. If a branch
  is touching `process_input_event`, `KeyboardFocusTarget`, or `focus.rs`,
  this work conflicts there. Browser+view are *clients*, so unlikely to
  touch any of this — but worth checking the diff of the branch with
  the terminal crate landing.
- **`shell/xdg.rs` `new_toplevel`**: spawn routing touches this. Less
  likely to conflict but possible.
- **Wayland protocol surface**: zero risk. Containers change what
  `configure` events say, not which events are sent.

Everything else (`render.rs`, `state.rs` shape changes, `camera.rs`,
`grid.rs`) is server-internal and invisible to client crates.

## Open questions to settle before writing code

1. **`Container` as enum vs trait object**: enum is simpler and matches
   the existing `KeyboardFocusTarget` / `PointerFocusTarget` style.
   Trait would let us add `Container` types later (e.g., a layer-shell
   adapter) without touching match arms. Default to enum unless a
   second container kind shows up in the design.
2. **Tab strip lives in SSD or as its own render element**? The
   existing SSD is per-window header. The tab strip is per-group. They
   feel like separate concerns — group decoration as its own
   `GroupDecoration` struct, not riding on SSD.
3. **What happens to a group when its last tab closes**? Group
   collapses: its grandparent (or `Space` root) replaces the group with
   its remaining child if there's one, or removes it entirely if there
   are zero. Confirm before implementing — the alternative is
   "groups stay around empty for re-spawn" but that's an Apple-Stage-
   Manager-style decision and probably wrong here.
4. **Drag a tab out of a group**: out of scope for the first PR. Note
   in the implementation that the tab-strip pointer handler should be
   stubbed with a TODO so we don't ship "tabs that look draggable but
   aren't."
