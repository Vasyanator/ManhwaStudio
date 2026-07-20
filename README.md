# ManhwaStudio

**English** · [Русский](docs/README.ru.md) · [Español](docs/README.es.md) · [Français](docs/README.fr.md) · [Português](docs/README.pt.md)

A specialized program for translating comics, including manga and webtoons. An all-in-one toolchain for downloading, pre-processing, translation, removing the original text and advanced typesetting. Unlike its alternatives, the program is focused on manual work rather than automation, and in many ways it is more convenient and intuitive than Photoshop.

> 📖 **The features are described in much more detail in the [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/en).** The most up-to-date version also ships inside the program itself.

**For suggestions, questions, support and community, join the Discord server or Telegram**:
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **To install, go to [Releases](https://github.com/Vasyanator/ManhwaStudio/releases/latest), download and run the executable for your system.** Windows, Linux and macOS are supported.

> **Note about the screenshots.** All screenshots below are taken with the Russian interface. Replacing them with English screenshots is a task waiting for a volunteer — pull requests are welcome.

## Core idea — text bubbles beside a continuous strip of pages. The whole chapter is processed at once. The bubbles point to the place where the translated text belongs.

# Main menu
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# New project window with the downloader
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

- Simply open a folder or an archive with a chapter
- Quick download from supported sites
- Download from most sites by writing the right image URL prefix
- Automatic download of all images with manual picking of the ones you need
- Extracting images from most sites by saving the page and opening the HTML
- Cropping when the pages are screenshots
- Stitching/slicing for webtoons. A smart algorithm that won't cut through artwork
- Running through Reline or Waifu2x for upscaling and noise removal

# PSD import window
<img width="2560" height="1356" alt="Image" src="https://github.com/user-attachments/assets/47602f13-1320-4d9e-ba71-b923c6d8b78f" />

- Open a folder or an archive with PSD files after cleaning
- Automatic detection of the page order and separation of the original from the cleaned layer
- Import the cleaned pages into the chapter

# Translation tab
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

- Text recognition via EasyOCR, MangaOCR, PaddleOCR, PaddleOCR-VL, SuryaOCR, AI API
- Creating and editing translation bubbles with character assignment
- Creating bubbles with images for AI translation, e.g. for sound effects
- Automatic text detection and machine translation if you just want a quick read
- Translation through the APIs of various AI services
- Composing the lines so they can be exported to docx or sent to an AI for a higher-quality translation. Sending to an AI requires character assignment

# Cleaning tab
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

- A fast brush with a color picker and the option to paint with a rectangle
- Pixel grid at high zoom levels
- AI removal for text on complex backgrounds using various AI models, from Lama to Flux
- An excellent gradient fill algorithm
- Texture synthesis
- Quick cleaning of detected text on uniform backgrounds

# Text tab
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />
<img width="2556" height="1341" alt="Image" src="https://github.com/user-attachments/assets/65951a3f-386d-4c49-96d2-581cfee0acd4" />

The most advanced part of the program.

- Select an area with Shift+LMB and quickly insert the text from the bubble that falls into the selection
  - A bubble is not required. You can just paste a line copied from a document with the translation
- Many text parameters, with individual adjustment of some parameters for only part of the text (via selection)
- Moving, rotating, scaling and warping the text image, a text layout line
- A text clipping mask with fill, letting you crop a text layer in a couple of clicks when it must sit under something
- Applying parameters to the text while it is still in vector form. For example rotation, stretching a character, forced bold/italic
- Various text settings and effects, including stroke, blur, glow, gradient, reflection and shake

# PS-like editor
<img width="2560" height="1343" alt="Image" src="https://github.com/user-attachments/assets/b5b5e83c-4ccb-4b1d-bb20-97a0e55218a8" />

Still in development, but it already lets you work with layers:
- Cut
- Move
- Slice into parts
- Draw

# Characters tab
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Here you can view and edit the title's characters. Their names and descriptions go into the instructions for the AI translation, to improve its understanding of the story and to keep names consistent.

# Glossary tab
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Same as characters, only without pictures.

# Translation notes tab
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

This is where the main instruction for the AI is written; characters and glossary entries are inserted into it automatically.
