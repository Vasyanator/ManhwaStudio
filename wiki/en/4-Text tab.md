# **Text tab**

**Note:** the screenshots are captured with the Russian interface. Retaking them in English is a task waiting for a volunteer — pull requests are welcome.

![image](../images/Вкладка-Текст/1.png)
Lets you place text images on the ribbon

## **How it works**
- You select the area for the text with `Shift+LMB`
- An editing window opens, filled with the text of the bubble that fell inside the selected area
- After the focus is lost (a click aside), the editing window closes and a text image is created with the chosen parameters
- The image can be moved by dragging 
- The image can be scaled with the `-` and `=` keys
- The image can be rotated by hovering over it and using `Ctrl+mouse wheel`
- The font size can be adjusted with `Shift+mouse wheel`

## **Parameters panel**
### **Text preview**
![image](../images/Вкладка-Текст/2.png)

Shows what the text will look like with the current parameters, ignoring the font size. Updates automatically.

### **Main text parameters**
![image](../images/Вкладка-Текст/2_1.png)

- `Preset`: Save and load the parameters you have set for each font.
- `Font`: Several fonts ship with the program, they are shown in the drop-down list. You can add your own by dropping a ttf/otf file into the fonts folder
- `face`: choosing a font from the family, if there is more than one
- `Use system fonts`: By default fonts are loaded only from the fonts folder next to the program installation
- `Font group`: Restrict the fonts shown to those that live in `fonts/groups/<group name>`. Fonts can be duplicated.
- `Size`: The font size
- `Line spacing`: The gap between text lines in pixels. Can be negative.
- `Kerning`: `Metric` - always the same distance between letters. `Auto` - use the character pairs from the font, for example `AV`, so that some characters sit closer. Does not work with every font.
- `Kerning X`: Extra distance between characters
- `Character height/width`
- `Alignment`: Left, right, center or free. You can move the slider for an intermediate position.
- `Global rotation`: Rotates the whole text while it is still in the vector state (not rasterized). Gives a noticeably sharper image than the ordinary rotation.
- `Shape`: The principle of laying out the lines, more on this below.
- `Wrapping`: Automatic word wrapping to keep the shape. It adjusts the strength of the wrapping or turns it off.
- `Anti-aliasing`: Same as anti-aliasing in Photoshop. Affects the sharpness of the text.
- `Allow moderate herringbone`: Affects the shape of the text. Allows dips in the shape.
- `Bold`: **Bold font**, does not work with every font
- `Italic`: Slanted font, does not work with every font
- `Hanging punctuation`: Punctuation characters are automatically not affected by the text alignment. The list of characters can be found in the settings, but almost all of them are there.
- `Strip extra spaces`: Removes spaces at the edges of the lines.
- `New line after sentence end`
- `All uppercase`
- `Parse tags`: Lets you mark a specific part of the text with `<b>` or `<i>` so that only part of the text becomes such. Example: `Only <b>one</b> word will be bold`. This is automated now, just select the text, change the available parameters, and it will be wrapped in tags.

### **Advanced text parameters**
![image](../images/Вкладка-Текст/2_2.png)

- `Line`: Can be horizontal (as usual) or vertical.
  - For the vertical one, both right-to-left and left-to-right are available.
- `Line layout`: Standard and formula. The formula lets you make any shape.

### **Additional text parameters**
Not always available.

#### **Parameters for the selected text only**
- `Offset X/Y`: The offset of a character or a group
- `Offset along line`: If the text uses a custom or formula layout, this shifts the selected characters along it
  - `Shift following characters`: Only for the offset along the line. The following text moves together with the selected fragment.
- `Character rotation`: Rotates every character separately within the selected text
- `Group rotation`: Rotates the whole selected text together
- `No break`: The text will not be broken by automatic wrapping.


## **Text editing panel**
![image](../images/Вкладка-Текст/13.png)


Appears when a text image is selected. Similar to the text creation panel, but it has a text field. The changes are applied immediately.

**A part of the text can be selected and the available parameters changed, and this automatically creates inline tags.**


## **Text shape**
Although it would be more correct to call this the quick shape.

The text tries to fit into this shape, avoiding herringbones and using smart wrapping.

The text can have these shapes:
- `Free`: Laid out as usual.
- `[ ]`: Square
- `( )`: Oval
- `< >`: Hexagonal
- The last 2 shapes have a minimum width parameter. The higher it is, the wider the top and the bottom of the text.

![image](../images/Вкладка-Текст/14.png)![image](../images/Вкладка-Текст/15.png)![image](../images/Вкладка-Текст/16.png)

As you can see, the shapes do not differ much. But better use the oval one.
Sometimes the wrapping strength is not enough to fit the text into the shape. Then play with the width, the font size, the minimum width, and the wrapping strength. Or pick a quick shape by right-clicking the text layer.

## **Advanced text shape**
![image](../images/Вкладка-Текст/21_1.png)
![image](../images/Вкладка-Текст/21_2.png)

Can be found on the editing panel.
It shows from a few to hundreds of thousands of possible shapes that have no herringbones and correct wrapping, which you can filter.
Unlike the quick shape, this shape is stable and will not change until you change it yourself or pick another one. It survives changes of the font and other parameters.

### Original and formed text
When an advanced shape is applied, the program takes the formed text, not the original one, to build the text layer. You can switch between them. The formed text can be edited by hand and tags can be added to it. But when a shape is picked again, the program will take the original text again to compute it, and when another shape is picked the formed text will be overwritten.


## **Text effects**

### **Stroke**
![image](../images/Вкладка-Текст/3.png)![image](../images/Вкладка-Текст/4.png)

Outlines the text with a line of the needed color and thickness

### **Glow**
![image](../images/Вкладка-Текст/5.png)![image](../images/Вкладка-Текст/6.png)

Makes an aura around the text

### **Shadow**
![image](../images/Вкладка-Текст/7.png)![image](../images/Вкладка-Текст/8.png)

Adds a shadow to the text with the specified X and Y offset

### **Gradient: 2 colors**
![image](../images/Вкладка-Текст/9.png)![image](../images/Вкладка-Текст/10.png)

Makes the text gradient in the needed direction

### **Gradient: 4 corners**
![image](../images/Вкладка-Текст/11.png)![image](../images/Вкладка-Текст/12.png)

Adds a gradient to the text based on four corners

### **Actions**
- `Refresh the source image` - reloads the current page on the canvas. Needed if you forgot to finish cleaning something.
- `Show text bubbles` - the option to hide the text bubbles on the sides, to judge the final translation better

### **Saving**
Saves the translated title in two ways. The scene render is recommended.


## **Clip mask**
![image](../images/Вкладка-Текст/17.png)![image](../images/Вкладка-Текст/18.png)
![image](../images/Вкладка-Текст/19.png)
![image](../images/Вкладка-Текст/20.png)

Lets you clip the text images. If a text image touches the mask, it is clipped, and only the parts under the mask remain visible. Clipping can be disabled for each text in the RMB menu.
It has a transparent yellow color and is not visible when this panel is closed.

### **Mask brush**

- Enabled by default while the panel is open
- `LMB` draws, `RMB` or `Shift+LMB` erases. The size is adjusted with `Ctrl+wheel`.

### **Mask fill**

- Activated by pressing the corresponding button on the mask panel
- On an LMB click it starts filling from the clicked point, spreading over a similar color within the tolerance


## **Text transform (perspective)**
![image](../images/Вкладка-Текст/22.png)![image](../images/Вкладка-Текст/22_1.png)

You can enter transform mode by right-clicking the selected text and choosing the corresponding item in the menu.
It lets you deform the text not only for perspective, dragging it by the handles.


## **Custom text layout**
![image](../images/Вкладка-Текст/23_1.png)![image](../images/Вкладка-Текст/23_2.png)![image](../images/Вкладка-Текст/23_3.png)

A useful thing for sound effects.
**To start, choose the "Custom text layout" item in the RMB menu of the text**

**This mode does not let the text lose focus, you have to press "Exit" deliberately**

- Change the text size by dragging the area by its corners
- Having selected a line on the panel, press LMB in the area to add its beginning
- With Shift+LMB drag the square point, leaving intermediate points behind, to set the shape of the line
- With LMB just drag the points, changing the shape without creating new ones
  - The big round point is the beginning of the line
  - The small round one is an intermediate point
  - The square point is the end, the line can be extended by it
  - If the points and the line are gray, it means another line is currently selected
    - If all the points and lines are gray, it means the selected line has not been created yet, press LMB to create the first point.

### **Mechanics**
- Each line of this layout takes the fragment of text between the line breaks. Several similar sound effects can be created at once

### **Panel**
![image](../images/Вкладка-Текст/23_4.png)

- Adding and deleting lines
- Smoothing (makes the line less angular)
- Direction and flipping of the text
- Minimum distance to a character: Just along the line, or do not allow characters to overlap


## **Adding your own fonts**
The program folder has a `fonts` folder with the 4 main fonts - Anime Ace, one for captions, one for sound effects, and Arial. You can drop your own `.ttf`/`.otf` files there.


## **Formula text layout**
Not sure why, but this exists.
The formula layout is located in `Advanced parameters`:
- `Layout`: `Standard` or `Formula`
- the formula fields `x`, `y`, `rotation`
- the `?` button after the formulas (show/hide the cheat sheet of variables and functions)
- the path parameters (`t_start/t_end`, `offset`, `scale`, `normal_offset`, `letter_spacing`)
- the constants `a..h`

### **How it works**
The program computes the position and the angle of **every character separately**.

For each character a parameter `t` is taken (usually from `0` to `1`), then:
- `x = formula_x(...)`
- `y = formula_y(...)`
- `rotation = formula_rotation(...)` (in radians)

After that the offsets/scales are applied and, if enabled, the rotation along the tangent to the path.

### **Parameters of the formula mode**
| Parameter | What it does | How to use it |
|---|---|---|
| `Layout` | Switches the mode | `Standard` for the standard layout, `Formula` for arcs/spirals/arbitrary paths |
| `x` | The formula of the X coordinate for each character | Basic start: `t * w` |
| `y` | The formula of the Y coordinate | Basic start: `120 * sin((t - 0.5) * pi)` |
| `rotation` | An extra rotation of each character | `0` for no extra rotation, `0.2*sin(2*pi*t)` for a wave |
| `?` | Shows/hides the hint | Handy when you need to recall the variables/functions quickly |
| `Tangent rotation` | Rotates the character along the direction of the curve | Enable it for arcs/spirals, disable it for "level" text on a curve |
| `t_start` | The start of the `t` parameter range | Usually `0` |
| `t_end` | The end of the `t` range | Usually `1`, increase it for a "longer" curve |
| `offset_x` | Shift of the whole path along X (px) | Move the whole caption right/left |
| `offset_y` | Shift of the whole path along Y (px) | Move the whole caption up/down |
| `scale_x` | Scale of the path along X | `>1` stretches, `<1` squeezes |
| `scale_y` | Scale of the path along Y | Controls the vertical amplitude |
| `normal_offset` | Shift of the characters along the normal to the curve | Useful to push the text outside/inside a circle |
| `letter_spacing` | Multiplier of the distance between characters | `1` is normal, `>1` spreads out, `<1` tightens |
| `a..h` | User constants for the formulas | Keep the "knobs" for amplitude, radius, number of turns and so on in them. But these are only numbers, you cannot enter a formula into them. |

### **Variables in the formulas**
- `t` — the current position within the range (`t_start..t_end`)
- `u` — the centered position (`-1..1`)
- `i` — the character index
- `n` — the number of characters
- `s` — the accumulated length along the line (in pixels)
- `line` — the line index
- `line_t` — the position of the character inside the current line (`0..1`)
- `line_n` — the number of characters in the current line
- `w` / `width` — the width of the text block (px)
- `fs` / `font_size` — the font size
- `a..h` — your constants from the UI
- `pi`, `tau`, `math_e` — mathematical constants

### **Available functions**
`sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `sqrt`, `abs`, `exp`, `ln`, `log`, `min`, `max`, `clamp`, `pow`, `rad`, `deg`, `floor`, `ceil`, `round`, `sign`

### **Important about the angle**
- `rotation` is given in **radians**
- if degrees are more convenient, use `rad(degrees)`, for example: `rad(25)`

### **Ready-made examples (copy them as they are)**
#### 1) Arc
- `x`: `t * w`
- `y`: `120 * sin((t - 0.5) * pi)`
- `rotation`: `0`
- `Tangent rotation`: `on`

#### 2) Slanted line
- `x`: `t * w`
- `y`: `0.35 * t * w`
- `rotation`: `0`
- `Tangent rotation`: `off`

#### 3) Wave
- `x`: `t * w`
- `y`: `80 * sin(2 * pi * t)`
- `rotation`: `0.15 * sin(2 * pi * t)`
- `Tangent rotation`: `off`

#### 4) Spiral (via `a`, `b`, `c`)
- `a = 40`, `b = 180`, `c = 3`
- `x`: `(a + b * t) * cos(c * tau * t)`
- `y`: `(a + b * t) * sin(c * tau * t)`
- `rotation`: `0`
- `Tangent rotation`: `on`

#### 5) Exponent
- `a = 3`
- `x`: `t * w`
- `y`: `140 * (exp(a * t) - 1) / (exp(a) - 1)`
- `rotation`: `0`
- `Tangent rotation`: `on`

### **Quick start (so as not to break the layout)**
Set the base values:
- `t_start = 0`
- `t_end = 1`
- `offset_x = 0`
- `offset_y = 0`
- `scale_x = 1`
- `scale_y = 1`
- `normal_offset = 0`
- `letter_spacing = 1`

And only then change one parameter at a time.
