# **Cleaning tab**

**Note:** the screenshots are captured with the Russian interface. Retaking them in English is a task waiting for a volunteer — pull requests are welcome.

![image](../images/Вкладка-Клининг/1.png)

Needed to clean the original text off the comic pages.
So far it has 2 tools - `Paint` and `AI removal`

## **Cleaning disappeared?**
Starting from 2.7, the structure has changed. The cleaning is now stored in the folder **projects/{title}/{chapter}/clean_layers** instead of **cleaned**. Just copy the images into the new folder.

## **Top bar**
- `Clear layer` - clears everything that was drawn
- `Show layer` - toggles the visibility of the drawing overlay. Lets you see what has just been painted and what was already on the source image.
- `Quick clean` - available if text detection was run, full description below.
- `Save cleaning` - saves the images into the project's cleaning folder

## **The Paint tool**
![image](../images/Вкладка-Клининг/2.png)

A fast brush with an eyedropper, an eraser and a rectangle. Suitable for covering text on a uniform background.

- The cursor shows the size of the drawing area and the current color (`red` in this case)
- `LMB` - normal drawing
- `RMB` - eyedropper, picks the color from under the center of the cursor
- `Shift+LMB` - eraser
- `Shift+mouse wheel` - brush size
- `Ctrl+LMB` - select and fill a rectangular area, for even faster cleaning

## **The AI removal tool**
![image](../images/Вкладка-Клининг/3.png)
![image](../images/Вкладка-Клининг/4.png)

Selects an area from the ribbon and removes the objects under the mask. Uses the AI from the `advimman/lama` repository

- With `Shift+LMB` select an area on the ribbon
  - **If the window did not open, it means the selection covered more than one page**
- A new window opens for drawing the mask
  - Draw the mask with `LMB`
  - Erase the mask with `RMB`
  - Change the brush size with `Shift+mouse wheel`
- The `Process` button runs the AI and removes the object under the mask
- You can enable `Refine`, it sometimes gives a slightly better result
- If something went wrong, you can press `Revert` and redraw the mask
- You can select with the mask again and remove the artifacts
- The `Close` button simply closes this window
- The `Apply` button puts the changed area onto the ribbon

### Other AI models

- `Lama MPE` - a smaller and slightly dumber Lama model from the zyddnys/manga-image-translator repository. But it sometimes works better with the anime style and comics than the regular Lama
- `AOT` - a very small model trained on manga. Also from zyddnys/manga-image-translator

## **The Gradient tool**
![image](../images/Вкладка-Клининг/5.png)
![image](../images/Вкладка-Клининг/6.png)

Selects an area from the ribbon and tries to restore the gradient under the mask. Often fills a gradient better than the AI

- The controls are the same as for the `AI removal` tool
- It does not break if a piece of solid color falls under the mask
- The program may freeze for a moment, that is normal

## **The Stamp tool**
![image](../images/Вкладка-Клининг/8.png)
![image](../images/Вкладка-Клининг/8_1.png)

It takes the area under itself from the same place of another image, and paints with it. 

Lets you take something from another version of the same chapter, for example the sound effects in English. Or remove watermarks using a version translated into any other language that does not have them.

### **To use this tool, you need to download and save an alternative version for this chapter in the downloader.**

It has these parameters:

- Source: The folder with the images. There can be several of them. In the project folder this lives in the alt_vers folder.
- Size: The brush size. Also adjustable with **Shift+mouse wheel**.
- Preview: Adjusts the opacity of the preview inside the brush circle.
- Y offset: Shifts the image the area is painted from up and down. Useful if banners were inserted there.

Controls:

- LMB: Draw
- RMB: Eraser
- Shift+LMB: Eraser (rectangular selection)
- Ctrl+LMB: Fill (rectangular selection)

## **Quick clean**
![image](../images/Вкладка-Клининг/7.png)
![image](../images/Вкладка-Клининг/7_1.png)

![image](../images/Вкладка-Клининг/7_2.png)

### **First run text detection in the Translation tab**

It uses the text mask produced by detection in the translation tab to try to paint over the text on a uniform background.
It only paints where the color along the edges of the mask is the same.

- `Mask auto-expansion` - by how much to expand the mask if the color turned out to be uneven. Helps to clean a bit more text. It triggers once.


## **How to do the cleaning in Photoshop?**
### Full cleaning
- Take the images from **projects/{title}/{chapter}/scr**
- Process them in Photoshop
- Save them into the folder **projects/{title}/{chapter}/clean_layers** and restart the program

### Process a difficult area
- Select the area with one of the area editing tools (OpenCV/Gradient/AI)
- Without changing anything, press **Apply**, this area will be transferred onto the transparent cleaning layer
- Press **Save layers**
- Open the corresponding image from the folder **projects/{title}/{chapter}/clean_layers** in Photoshop
- Process it, save it and restart the program
