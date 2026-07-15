# **Image ribbon:**

**Note:** the screenshots are captured with the Russian interface. Retaking them in English is a task waiting for a volunteer — pull requests are welcome.

![alt text](<../images/1-Лента картинок и её параметры/image.png>)
![alt text](<../images/1-Лента картинок и её параметры/image-1.png>)

All pages are shown at once here. Manga and paged comics have a gap between pages; webtoons do not. Even so, it is better not to cut a bubble across two pages — stitching and slicing in the new project window exist for that.

# **Controlling the ribbon**
The ribbon behaves like one big canvas: you can scroll it, pan it, zoom it, and create bubbles right on the page.

### Scrolling and panning
The mouse wheel scrolls the ribbon up and down. If the ribbon is wider than the window because of zoom or side bubbles, a horizontal scrollbar appears at the bottom.

To pan the ribbon by hand, hold `Space` and drag with the mouse. This is handy at high zoom, when you need to shift the canvas sideways quickly.

There is a small floating panel in the top left. It shows the current page, the zoom level, a toggle for showing bubbles, and the bubble opacity. The panel can be collapsed and dragged.

### Zoom
Zoom happens around the point under the cursor, so the ribbon does not jump somewhere else when you zoom in.

- `Ctrl` + mouse wheel — zoom in or out.
- `Z` + mouse wheel — the same, but without `Ctrl`.
- `Ctrl` + `+` / `Ctrl` + `-` — zoom in or out.
- `Ctrl` + `0` — reset the zoom.
- `Z` + `+` / `Z` + `-` / `Z` + `0` — the same zoom commands via the `Z` key.
- `Ctrl` or `Z` + hold the left mouse button and move left/right — smooth zoom by dragging.

On macOS, `Cmd` is usually used instead of `Ctrl`.

### Creating and selecting bubbles
In the `Translation` tab, a new bubble can be created with `T` at the cursor position. The same action is available in the page context menu on the right mouse button.

A left click on empty space deselects the bubble. A left click on a bubble selects it. `Delete` removes the selected bubble.

The right mouse button on the page opens the menu for creating and pasting bubbles. The right mouse button on a bubble opens the bubble's own menu: it has copy, paste, duplicate and type-change actions, plus extra items if spellchecking is enabled.

### Moving bubbles
`On top` bubbles can be moved right on the page, and resized by the handles on their edges.

`Aside` bubbles live in the side column but are anchored to a point on the page. They can be dragged, and the anchor area can be moved on the image itself. A line shows which spot a side bubble belongs to.

### Undo and redo
For bubble actions, the following work:

- `Ctrl` + `Z` — undo.
- `Ctrl` + `Shift` + `Z` — redo.
- `Ctrl` + `D` — duplicate the selected bubble.

All of these hotkeys can be reassigned in the hotkey settings.

# **Ribbon settings**
The ribbon has many settings; they are located in the corresponding tab > `Ribbon and bubbles`.
![](<../images/1-Лента картинок и её параметры/image-2.png>)

### Settings preset
Lets you quickly apply the standard settings for webtoons or paged comics.

- **Paged** — pages are separated by a gap, side bubbles in the translation tab are shrunk more aggressively.
- **Webtoon** — pages go one right after another, side bubbles are not shrunk.
- **Custom** — turns on if the parameters differ from the standard presets.

This does not change the images themselves, only the behavior of the ribbon and the bubbles.

### Default bubble type in the translation/cleaning/text tab
Determines whether standard bubbles are displayed as `On top` or `Aside`.

In the translation tab this affects new bubbles and regular bubbles without an explicit type. In the cleaning and text tabs it affects how already existing standard bubbles are shown in view mode.

If you choose `Aside`, the bubble sits in the side column and is connected to a spot on the page by a line. If you choose `On top`, the bubble lies right on the page.

### Automatically insert the last character
If enabled, a newly created bubble immediately gets the last selected character. Handy when several lines of the same character follow each other.

### Spellcheck the original / translation
Enables spelling highlights in the corresponding bubble fields. For the translation it is usually worth keeping on; for the original it depends: OCR often produces names, slang and fragments of another language, which the dictionary will treat as errors anyway.

### Custom spellcheck words
Here you can add words that should not be highlighted as errors.

- **Shared exclusions** work for all projects.
- **Project exclusions** are stored only for the current chapter/project.

Write one word per line. This is handy for names, terms, technique names, cities, and words the dictionary does not know.

### Stretch side bubbles
Controls the width of the bubbles located next to the page.

If enabled, the width of a side bubble adapts to the free space beside the page, but stays within the minimum and maximum width. That is, the bubble tries not to go off screen if there is room for it.

If disabled, side bubbles always take the minimum width.

### Shrink side bubbles in the translation tab
This setting exists so that a ribbon with a lot of bubbles does not turn into a giant wall of interface.

- **None** — the bubble is always fully expanded: original, translation, buttons, number, character.
- **Moderate** — while the bubble is not selected, only the original and translation lines are visible. When focused, it expands fully.
- **Strong** — while the bubble is not selected, only the translation line is visible. If the translation is empty, the original is shown. When focused, it expands fully.

For webtoons **None** is usually more convenient, because pages form one continuous ribbon. For paged manga **Strong** is often more convenient, because the side columns take up less space.

### Side of the side bubbles
Determines where `Aside` bubbles are shown.

- **Auto** — the bubble appears on the left or on the right depending on its position on the page.
- **Left** — all side bubbles go on the left.
- **Right** — all side bubbles go on the right.

In **Auto** mode you can move the bubble's anchor on the page, and the side will follow its position. The forced modes are handy if you want to keep the whole translation in a single column.

### Expansion of "On top" bubbles
`On top` bubbles lie right on the page, inside their text rectangle. This setting decides what to do with the extra interface when such a bubble is selected.

- **Around** — the bubble stays on top of the page, the original is shown above, the character and buttons below.
- **Aside** — the selected bubble temporarily expands as a side bubble. This is handy when you do not want to cover the artwork with buttons and fields.

### Side bubble size (%)
Scales the side interface: text, buttons, padding and the column itself. 100% is the normal size. Below 100% makes side bubbles more compact, above 100% makes them bigger.

### Min. and max. side bubble width
These are the bounds of the side column width.

- **Min. width** — a side bubble will not get narrower than this.
- **Max. width** — a side bubble will not stretch wider than this.

If the maximum width accidentally becomes smaller than the minimum, the program raises it to the minimum.

### Separate pages
If enabled, a separate gap appears between the images. This is the normal mode for paged manga and regular comics.

If disabled, pages go right next to each other. This is the webtoon mode, where the whole chapter reads as one long vertical ribbon.

### Page spacing
Only works when **Separate pages** is enabled. The larger the value, the larger the distance between neighboring pages.

For webtoons this parameter is not needed, because pages are not separated.

### Top/bottom margin
Adds empty space at the beginning and at the end of the ribbon. It does not change the images, it only gives comfortable scrolling headroom so the first and last pages do not stick to the window edge.

### Auto-sync between tabs
Synchronizes the ribbon position between the `Translation`, `Cleaning` and `Text` tabs. If enabled, you can switch to another tab and stay at roughly the same place in the chapter.

If disabled, each tab keeps its own scroll position.

### Cache pages
If enabled, the program keeps decoded pages in memory in advance for fast operations. This speeds up cleaning, export and other actions that need the source pixels.

If you are short on memory or the chapter is very large, you can turn it off. Then the program keeps less in memory, but some actions may open more slowly.

### Bubble status
Statuses draw a colored border around bubbles according to rules. This helps you quickly see which lines are not ready yet.

Rules are applied top to bottom: the first matching rule chooses the border style. A rule has a condition and a border.

Conditions can be assembled from blocks:

- **Translation filled**
- **Original filled**
- **Character filled**
- **AND** — all nested conditions must match
- **OR** — one nested condition is enough
- **NOT** — inverts the nested condition

For the border you can choose the type: solid, dashed, dotted or wavy, as well as the color.

The default preset shows a red border if the translation is not filled, and a green border if the translation and the character are filled.
