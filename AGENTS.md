# Web player interactions

Give player controls immediate visual feedback. Use short transitions in both directions when menus and dialogs open or close, sections expand or collapse, and items appear or disappear. Match the pace of nearby controls, typically 120–160 ms, and keep the surrounding layout steady.

Honor `prefers-reduced-motion` and keep controls responsive to repeated clicks and keyboard input during transitions.

Build player dialogs with the shared native `player-dialog` and `player-dialog-surface` styles. Use `player/dialog_header.html` for the title bar and `player/dialog_close.html` for the close control in custom headers. Keep feature-specific styles in the dialog body.

When a view or section waits for its first data, show a quiet skeleton matching its final layout, including on mobile. Reuse the player's card and track placeholders. Keep existing content visible during background refreshes and pagination. Replace placeholders as soon as data, an empty result, or an error arrives. Announce loading once to assistive technology and keep decorative shapes hidden from it.
