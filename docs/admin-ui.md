# Admin interface conventions

[Project overview](../README.md) · [Development](development.md)

The admin UI uses one Alpine application and shared layout primitives for every
section. Add features to this model rather than introducing a separate mobile
page or section-specific navigation.

## Ownership

- `templates/admin/navigation.html`: the single navigation tree, grouped into
  Operations (reviews/tasks), Music (library/tools/statistics), and Instance
  (users/settings). Existing hash routes remain the source of navigation state.
- `templates/admin/styles.html`: colors, typography and component appearance.
- `templates/admin/responsive.html`: shared layout, breakpoints, touch targets,
  scroll regions, table cards and dialog sizing. Put responsive rules here.
- `templates/admin/ui.html`: viewport state, task tabs, settings navigation and
  the shared overlay lifecycle. Business data and API actions remain in
  `adminV2()` in `v2.html`.

## Layout

| Width | Navigation | Workspaces | Lists and dialogs |
| --- | --- | --- | --- |
| 1280 px and above | Persistent sidebar | Task list/detail/log side by side | Tables, centered bounded dialogs |
| 1024–1279 px | Persistent sidebar | Task tabs; settings in one column | Tables, bounded dialogs |
| 768–1023 px | Drawer | Task tabs; settings section picker | Tables in contained scroll areas |
| Below 768 px | Drawer | Single column | Table cards, full viewport dialogs |

Keep CSS and `matchMedia` breakpoints in `ui.html` synchronized. The main panel
owns page scrolling. Use `minmax(0, 1fr)` and `min-width:0` for grid/flex children;
never restore a global minimum page width. The top bar stays available while
content scrolls. Navigation closes after route changes and when switching to
the desktop layout. It retains the active section and the existing URL routes.

Use `.panel`, `.panel-head`, `.panel-title`, `.toolbar`, `.action-strip`, `.field`
and `.editor-grid` for new content. Toolbars wrap, inputs fill available space,
and mobile controls use 44 px touch targets and 16 px input text. Long identifiers,
paths and names must wrap. Avoid fixed column widths and inline grid definitions.

For tabular lists, add `.responsive-table`, a `data-label` to every cell, and
`data-primary` to its identifying cell. Mobile CSS presents the same DOM as cards;
do not duplicate rows or hide actions. Give clickable row titles real buttons
using `.row-link`, so the operation also works with a keyboard. Dense comparisons,
charts and logs may scroll inside a named, keyboard-focusable region; horizontal
scrolling must never propagate to the page. Keep these exceptions intentional.

Task tabs change presentation only: configuration and selected run stay in memory
when switching tabs or rotating the device. In settings, add a section ID and a
matching entry to the section picker. Put related controls in `.settings-grid`.

## Dialogs

Every dialog uses the same hierarchy and lifecycle:

```html
<div class="modal-backdrop" x-show="exampleOpen"
     x-effect="adminOverlay.sync($el, !!exampleOpen)"
     @keydown="adminOverlay.keydown($event, $el)"
     @click.self="closeExample()">
    <section class="modal" role="dialog" aria-modal="true"
             aria-labelledby="example-title" tabindex="-1">
        <div class="modal-head">
            <strong id="example-title">Edit item</strong>
            <button class="icon-btn" data-dialog-close
                    aria-label="Close dialog" @click="closeExample()">…</button>
        </div>
        <form class="modal-body" @submit.prevent="saveExample()">
            <!-- .field and .editor-grid -->
            <div class="dialog-actions"><button type="submit" class="btn primary">Save</button></div>
        </form>
    </section>
</div>
```

The helper locks the background with `inert`, moves focus into the dialog, traps
Tab/Shift+Tab, handles Escape through the existing close button, and restores
focus on close. Keep business rules in the close handler and disable that button
when closing is not allowed (for example, while merging). Nested pickers consume
Escape with `.stop` before the dialog handles it. Headers remain fixed while the
body scrolls; `.dialog-actions` keeps actions available at the bottom. On mobile,
dialogs use `100dvh` and safe-area padding. Reuse the helper for new overlays.

For content gated by an asynchronous load inside an animated dialog, use
`:hidden="loading"` instead of nesting competing `x-show` transitions. Loading
and content should not both stay hidden when a response arrives during opening.

## Verification

Run `node --test tests/*.test.cjs` and build the Rust application (templates are
compiled into the binary). `tests/browser-admin-responsive.cjs` checks all seven
sections at 320, 390, 768, 1024 and 1440 px, navigation focus, task state retention,
settings jumps, all five dialogs, long content and landscape geometry. It uses a
disposable running instance with an admin and at least one release containing a
track; browser fixtures do not modify server data. Supply `FURU_TEST_URL`,
`FURU_TEST_USERNAME`, `FURU_TEST_PASSWORD`, `PLAYWRIGHT_MODULE`, and `CHROME_PATH`
as appropriate. Set `FURU_TEST_SCREENSHOTS` to inspect screenshots visually.

Check both populated and empty views, long names/paths, loading states, keyboard
navigation, reachable Save/Close controls, and resize while an overlay is open.
