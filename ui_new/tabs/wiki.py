import os
import sys
import glob
import requests

from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QTabWidget,
    QTextBrowser, QWidget, QVBoxLayout, QMessageBox, QHBoxLayout
)
from PyQt6.QtGui import QImage, QTextDocument
from PyQt6.QtCore import QUrl, Qt

import markdown
def styledhtml(html):
    return f"""
        <html>
        <head>
        <style>
            body {{
                margin: 0;
                padding: 0;
            }}
            img {{
                max-width: 100%;
                height: auto;
            }}
        </style>
        </head>
        <body>
        {html}
        </body>
        </html>
        """

class MarkdownViewer(QTextBrowser):
    def __init__(self, base_path: str, parent=None):
        super().__init__(parent)
        self.base_path = base_path
        self.setOpenExternalLinks(True)
        self.setReadOnly(True)

    def loadResource(self, type: int, name: QUrl):
        if type == int(QTextDocument.ResourceType.ImageResource):
            img = QImage()

            # Локальные/относительные пути
            if name.isLocalFile() or name.scheme() == "":
                local_path = name.toLocalFile()
                if not local_path:
                    local_path = os.path.join(self.base_path, name.toString())
                img = QImage(local_path)
                if img.isNull():
                    print(f"Не удалось загрузить локальное изображение: {local_path}")

            # http/https
            elif name.scheme() in ("http", "https"):
                url = name.toString()
                try:
                    resp = requests.get(url, timeout=10)
                    resp.raise_for_status()
                    if not img.loadFromData(resp.content):
                        print(f"Не удалось декодировать изображение из {url}")
                except Exception as e:
                    print(f"Ошибка загрузки изображения {url}: {e}")
                    img = QImage()

            # Возвращаем как есть — размеры ограничит CSS (max-width: 100%)
            if not img.isNull():
                return img

            return QImage()

        return super().loadResource(type, name)

class MarkdownFileWidget(QWidget):
    """
    Виджет для просмотра одного Markdown-файла (его можно сразу добавлять как вкладку).
    """
    def __init__(self, filepath: str, parent=None):
        super().__init__(parent)
        self.filepath = filepath

        try:
            with open(filepath, "r", encoding="utf-8") as f:
                md_text = f.read()
        except Exception as e:
            md_text = f"Ошибка чтения файла {filepath}: {e}"

        html = markdown.markdown(
            md_text,
            extensions=["fenced_code", "tables", "toc"]
        )

        base_path = os.path.dirname(filepath) or os.getcwd()

        viewer = MarkdownViewer(base_path)
        base_url = QUrl.fromLocalFile(base_path + os.sep)
        viewer.document().setBaseUrl(base_url)
        viewer.setHtml(styledhtml(html))

        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addStretch(1)
        layout.addWidget(viewer, 8)
        layout.addStretch(1)

class MarkdownFolderWidget(QWidget):
    """
    Виджет для папки: внутри собственный QTabWidget со всеми .md в директории.
    Его тоже можно добавлять как вкладку в ваш внешний QTabWidget.
    """
    def __init__(self, folder: str, parent=None):
        super().__init__(parent)
        self.folder = folder

        tabs = QTabWidget(self)

        pattern = os.path.join(folder, "*.md")
        files = sorted(glob.glob(pattern))

        if not files:
            # Пустая заглушка
            placeholder = QTextBrowser()
            placeholder.setHtml(styledhtml(
                f"<h3>В папке '{folder}' нет .md файлов</h3>"
            ))
            tabs.addTab(placeholder, "—")
        else:
            for path in files:
                title = os.path.splitext(os.path.basename(path))[0]
                tabs.addTab(MarkdownFileWidget(path), title)

        lay = QVBoxLayout(self)
        lay.setContentsMargins(0, 0, 0, 0)
        lay.addWidget(tabs)

def create_markdown_widget(path: str) -> tuple[QWidget, str]:
    """
    Универсальная фабрика:
      - если path — файл .md => возвращает (MarkdownFileWidget, 'Имя_файла')
      - если path — папка     => возвращает (MarkdownFolderWidget, 'Имя_папки')
    """
    if os.path.isfile(path):
        title = os.path.splitext(os.path.basename(path))[0]
        return MarkdownFileWidget(path), title
    elif os.path.isdir(path):
        title = os.path.basename(os.path.normpath(path)) or path
        return MarkdownFolderWidget(path), title
    else:
        # Фолбэк: информируем и отдаем пустой viewer
        w = QTextBrowser()
        w.setHtml(styledhtml(f"<h3>Путь не найден:</h3><pre>{path}</pre>"))
        return w, "Ошибка"


class MainWindow(QMainWindow):
    def __init__(self, path: str, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Markdown Wiki Viewer (PyQt6)")

        tabs = QTabWidget()
        self.setCentralWidget(tabs)

        # Если пришла папка — заводим одну вкладку с папкой.
        # Если файл — одну вкладку с файлом.
        widget, title = create_markdown_widget(path)
        tabs.addTab(widget, title)

def _prepare_input_path(path: str) -> str:
    """
    Возвращает абсолютный путь. Создаёт папку только если path — папка (или не существует и это не .md).
    Если path — .md файл и его нет, создаём родительскую папку (а файл — по желанию).
    """
    path = os.path.abspath(path)

    if os.path.isdir(path):
        # уже папка — ок
        return path

    if os.path.isfile(path):
        # уже файл — ок
        return path

    # Путь не существует: решаем по расширению
    if path.lower().endswith(".md"):
        # это новый файл: создадим родительскую директорию, сам файл не трогаем
        parent = os.path.dirname(path) or "."
        os.makedirs(parent, exist_ok=True)
        return path
    else:
        # это новая папка
        os.makedirs(path, exist_ok=True)
        return path



def main():
    app = QApplication(sys.argv)
    QApplication.setAttribute(Qt.ApplicationAttribute.AA_Use96Dpi)

    if len(sys.argv) > 1:
        # Пользователь задал путь (файл или папка)
        input_path = _prepare_input_path(sys.argv[1])
    else:
        # Поведение по умолчанию — папка wiki рядом со скриптом
        script_dir = os.path.abspath(os.path.dirname(__file__))
        input_path = os.path.join(script_dir, "wiki/Вкладка-Клининг.md")
        if not os.path.isdir(input_path):
            os.makedirs(input_path, exist_ok=True)
            readme_path = os.path.join(input_path, "README.md")
            if not os.path.exists(readme_path):
                with open(readme_path, "w", encoding="utf-8") as f:
                    f.write(
                        "# Добро пожаловать в wiki\n\n"
                        "Создайте здесь свои `.md` файлы.\n\n"
                        "Пример картинки из интернета:\n\n"
                        "![Кот](https://placekitten.com/300/200)\n"
                    )

    window = MainWindow(input_path)
    window.resize(1000, 700)
    window.show()

    sys.exit(app.exec())


if __name__ == "__main__":
    main()
