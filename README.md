# ManhwaStudio

**English** · [Русский](docs/README.ru.md) · [Español](docs/README.es.md) · [Français](docs/README.fr.md) · [Português](docs/README.pt.md)

A simple mini-studio for translating manhwa on your own. It works well for less popular titles without complicated cleaning; complex titles are better left to teams working in Photoshop.

> 📖 **The features are described in much more detail in the [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/en).** The most up-to-date version also ships inside the program itself.

**For suggestions, questions, support and community, join the Discord server or Telegram**:
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Note about the screenshots.** All screenshots below are taken with the Russian interface. Replacing them with English screenshots is a task waiting for a volunteer — pull requests are welcome.

## Core idea — text bubbles beside a continuous manhwa strip. The bubbles point to the place where the translated text belongs.

# Main menu
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# New project window with the downloader
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

Here you get downloading and initial processing options. You can simply open a folder, point to a chapter on one of the supported sites, use the browser with URL templates, or open an offline copy of a page.
After that, you can stitch and slice the strip (for vertical comics) and remove noise with Waifu2x.

# Translation tab
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

This is where translation and editing happen. Text can be recognized in many languages via EasyOCR, PaddleOCR or MangaOCR. Each line can be assigned a role, which is used when composing the text before sending it to the AI. Alternatively, you can just run text detection and push everything through machine translation if you only want to read.

# Cleaning tab
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

This is where the original text is painted over. The feature set is fairly modest compared to Photoshop, but painting over a uniform background is quite convenient. AI models for removing objects under a mask, and the gradient restoration tool, give quite decent quality. On top of that, particularly difficult fragments can be handled separately in Photoshop.

# Text tab
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />

This is where the translated text is placed. Shift+LMB selects an area for it, and if a text block on the side points here, the text is inserted automatically. You can also type it in by hand.
The panel offers various effects, from shadow and stroke to gradients. The text images themselves can be cropped with a clipping mask or transformed in perspective. Typesetting is significantly faster than in Photoshop.

# Characters tab
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Here you can view and edit the title's characters. Their names and descriptions go into the instructions for the AI translation, to improve its understanding of the story and to keep names consistent.

# Glossary tab
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Same as characters, only without pictures.

# Translation notes tab
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

This is where the main instruction for the AI is written; characters and glossary entries are inserted into it automatically.
