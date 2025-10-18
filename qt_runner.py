# qt_runner.py
import sys, argparse, os
from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QTabWidget, QSplashScreen
)
from PyQt6.QtCore import Qt, QTimer, pyqtSignal
from PyQt6.QtGui import QPixmap, QIcon
from modules.project import Project
import importlib, traceback, types, weakref
from PyQt6.QtWidgets import QWidget, QVBoxLayout, QPushButton, QMessageBox, QLabel, QPlainTextEdit
from ui_new.models.bubbles_model import BubblesModel
VERSION = "2.0"

class SystemTab(QWidget):
    def __init__(self, owner: "AppWindow"):
        super().__init__()
        self.owner = owner
        lay = QVBoxLayout(self)

        lay.addWidget(QLabel("Перезагрузка вкладок без перезапуска приложения."))

        # Кнопки по вкладкам
        for key in ["Перевод", "Клининг", "Текст", "Персонажи", "Термины", "Заметки перевода"]:
            btn = QPushButton(f"Перезагрузить: {key}")
            btn.clicked.connect(lambda _, k=key: self.owner.reload_tab(k))
            lay.addWidget(btn)

        # Кнопка «всё сразу»
        btn_all = QPushButton("Перезагрузить все вкладки")
        btn_all.clicked.connect(self.owner.reload_all_tabs)
        lay.addWidget(btn_all)

        lay.addStretch(1)

class ErrorTab(QWidget):
    def __init__(self, tab_name: str, tb_text: str, on_reload):
        super().__init__()
        self._tab_name = tab_name
        lay = QVBoxLayout(self)
        lay.addWidget(QLabel(f"Вкладка «{tab_name}» упала с исключением."))
        btn = QPushButton("Перезагрузить вкладку")
        btn.clicked.connect(lambda: on_reload(tab_name))
        lay.addWidget(btn)

        log = QPlainTextEdit()
        log.setReadOnly(True)
        log.setPlainText(tb_text)
        lay.addWidget(log)
        lay.addStretch(1)


class AppWindow(QMainWindow):
    def __init__(self, project_path: str):
        super().__init__()
        self.project_path = project_path
        self.project = Project(project_path)
        self.bubbles_model = None
        chapter = os.path.basename(project_path)
        comic = os.path.basename(os.path.dirname(project_path))
        self.setWindowTitle(f"ManhwaStudio v{VERSION}: {comic}, {chapter}")
        self.setWindowIcon(QIcon("app_icon_512.png"))
        self.showMaximized()

        # Поставим пустой таб-виджет, чтобы окно отрисовалось мгновенно
        self.tabs = QTabWidget()
        self.tabs.setTabPosition(QTabWidget.TabPosition.North)
        self.setCentralWidget(self.tabs)
        # Реестр вкладок: имя -> данные
        self.tab_widgets: dict[str, QWidget] = {}
        self.tab_indexes: dict[str, int] = {}
        # Маппинг: имя вкладки -> (module_name, class_name)
        self.tab_classes = {
            "Перевод": ("ui_new.tabs.translation_tab_qt", "TranslationTab"),
            "Клининг": ("ui_new.tabs.cleaning_tab_qt", "CleaningTab"),
            "Текст": ("ui_new.tabs.text_tab.text_tab_qt", "TextEditorTabQt"),
            "Персонажи": ("ui_new.tabs.characters_tab", "CharactersTab"),
            "Термины": ("ui_new.tabs.terms_tab", "TermsTab"),
            "Заметки перевода": ("ui_new.tabs.notes_tab", "TranslationNotesTabQt"),
        }


    # Тяжёлая инициализация вызывается ПОСЛЕ показа окна
    def heavy_init(self, splash: QSplashScreen | None = None, app: QApplication | None = None):
        def msg(text):
            if splash:
                splash.showMessage(text, alignment=Qt.AlignmentFlag.AlignHCenter | Qt.AlignmentFlag.AlignBottom)
                if app:
                    app.processEvents()

        msg("Загрузка проекта…")
        if self.project.exists():
            self.project.load()
            self.bubbles_model = getattr(self.project, "_bubbles_model", None) or BubblesModel(self.project)
            self.project._bubbles_model = self.bubbles_model
        else:
            # Можно показать предупреждение
            pass

        msg("Подготовка вкладок: Перевод…")
        self._create_and_add_tab("Перевод")
        app and app.processEvents()

        msg("Клининг…")
        self._create_and_add_tab("Клининг")
        app and app.processEvents()

        msg("Текстовый редактор…")
        self._create_and_add_tab("Текст")
        app and app.processEvents()

        msg("Персонажи…")
        self._create_and_add_tab("Персонажи")
        app and app.processEvents()

        msg("Термины…")
        self._create_and_add_tab("Термины")
        app and app.processEvents()

        msg("Заметки перевода…")
        self._create_and_add_tab("Заметки перевода")
        app and app.processEvents()

        # Вкладка «Системное» — в конце
        self._create_system_tab()

    def _construct_tab_widget(self, name: str) -> QWidget:
        """
        Создаёт экземпляр виджета вкладки по имени, учитывая зависимости.
        """
        mod_name, cls_name = self.tab_classes[name]
        # Модуль уже импортирован (вы делали from ... import ...), достанем его из sys.modules
        module = sys.modules.get(mod_name)
        if module is None:
            module = importlib.import_module(mod_name)
        # Берём актуальный класс
        cls = getattr(module, cls_name)

        # Конструируем с учётом зависимостей
        if name == "Заметки перевода":
            # Эта вкладка зависит от текущих экземпляров «Персонажи» и «Термины»
            charas = self.tab_widgets.get("Персонажи")
            terms  = self.tab_widgets.get("Термины")
            return cls(self.project, charas_tab=charas, terms_tab=terms)

        elif name in ("Перевод", "Клининг", "Текст"):
            # Этим вкладкам нужен общий BubblesModel
            return cls(self.project, bubbles_model=self.bubbles_model)

        else:
            return cls(self.project)

    def _create_and_add_tab(self, name: str):
        widget = self._construct_tab_widget(name)
        idx = self.tabs.addTab(widget, name)
        self.tab_widgets[name] = widget
        self.tab_indexes[name] = idx

    def _create_system_tab(self):
        sys_tab = SystemTab(self)
        idx = self.tabs.addTab(sys_tab, "Системное")
        self.tab_widgets["Системное"] = sys_tab
        self.tab_indexes["Системное"] = idx

    def reload_tab(self, name: str):
        """
        Перезагружает модуль, пересоздаёт виджет и подменяет вкладку.
        Если перезагружаются «Персонажи» или «Термины», автоматически
        пересоберём «Заметки перевода», чтобы не остаться со старыми ссылками.
        """
        try:
            if name not in self.tab_classes and name != "Системное":
                raise RuntimeError(f"Неизвестная вкладка: {name}")

            # 1) Перезагрузка модулей (с учётом зависимостей)
            to_reload = [name]
            if name in ("Персонажи", "Термины"):
                # «Заметки перевода» зависят — пересоберём их тоже
                if "Заметки перевода" not in to_reload:
                    to_reload.append("Заметки перевода")

            for n in to_reload:
                if n == "Системное":
                    continue
                mod_name, _ = self.tab_classes[n]
                module = sys.modules.get(mod_name)
                if module is None:
                    module = importlib.import_module(mod_name)
                importlib.reload(module)

            # 2) Пересоздание и замена виджетов
            for n in to_reload:
                if n == "Системное":
                    continue
                old_widget = self.tab_widgets.get(n)
                idx = self.tab_indexes.get(n)
                if idx is None:
                    continue

                new_widget = self._construct_tab_widget(n)
                # Важно: заменить виджет на месте, чтобы сохранить порядок и заголовок
                self.tabs.removeTab(idx)
                # После removeTab индексы вправо сдвинутся — вычислим новое место:
                # Найдём, где стояла вкладка по имени среди текущих
                # (проще — вставим обратно на min(idx, current_count))
                idx_insert = min(idx, self.tabs.count())
                self.tabs.insertTab(idx_insert, new_widget, n)

                # Обновим реестры
                self.tab_widgets[n] = new_widget
                self.tab_indexes[n] = idx_insert

                # Уничтожим старый виджет корректно
                if old_widget is not None:
                    old_widget.deleteLater()

            QMessageBox.information(self, "Готово", f"Вкладка(и) «{', '.join(to_reload)}» перезагружены.")
        except Exception as e:
            traceback.print_exc()
            QMessageBox.critical(self, "Ошибка", f"Не удалось перезагрузить «{name}»:\n{e}")

    def reload_all_tabs(self):
        # Последовательная перезагрузка всех рабочих вкладок, затем пересборка «Заметок»
        for name in ["Перевод", "Клининг", "Текст", "Персонажи", "Термины", "Заметки перевода"]:
            self.reload_tab(name)

    def handle_ui_exception(self, receiver, exc: Exception, tb_str: str):
        """
        Пытается найти, к какой вкладке относится источник события (receiver),
        и заменяет её на ErrorTab. Если не нашли — покажем ошибку активной вкладке.
        """
        # 1) Найдём QWidget-страницу вкладки
        page_widget = None
        obj = receiver
        # Поднимемся по родителям, пока не встретим один из виджетов вкладок
        seen = set()
        while obj and obj not in seen:
            seen.add(obj)
            if isinstance(obj, QWidget) and obj in self.tab_widgets.values():
                page_widget = obj
                break
            obj = getattr(obj, "parent", None)() if isinstance(obj, weakref.ReferenceType) else getattr(obj, "parent", None)
            if callable(obj):  # иногда parent() — метод у QObject
                obj = obj()
        # 2) Определим имя вкладки
        tab_name = None
        for name, w in self.tab_widgets.items():
            if w is page_widget:
                tab_name = name
                break
        if tab_name is None:
            # fallback: активная вкладка
            idx = self.tabs.currentIndex()
            tab_name = self.tabs.tabText(idx)

        # «Системное» не трогаем — заменим только рабочие вкладки
        if tab_name == "Системное":
            return

        # 3) Подменим вкладку на ErrorTab
        idx = self.tab_indexes.get(tab_name)
        if idx is None:
            idx = self.tabs.indexOf(self.tab_widgets.get(tab_name))
        if idx < 0:
            idx = self.tabs.currentIndex()

        err = ErrorTab(tab_name, tb_str, on_reload=self.reload_tab)
        old = self.tab_widgets.get(tab_name)
        self.tabs.removeTab(idx)
        self.tabs.insertTab(idx, err, tab_name + " (ошибка)")
        self.tabs.setCurrentIndex(idx)

        # обновим реестры
        self.tab_widgets[tab_name] = err
        self.tab_indexes[tab_name] = idx

        # корректно уничтожим старый виджет
        if old is not None and isinstance(old, QWidget):
            old.deleteLater()


class SafeApplication(QApplication):
    def __init__(self, *args, on_exception=None, **kwargs):
        super().__init__(*args, **kwargs)
        self._on_exception = on_exception  # callable(receiver, exc, tb_str)

    def notify(self, receiver, event):
        try:
            return super().notify(receiver, event)
        except Exception as e:
            tb_str = "".join(traceback.format_exception(type(e), e, e.__traceback__))
            if self._on_exception:
                self._on_exception(receiver, e, tb_str)
            # Возврат True говорит Qt: «событие обработано» — продолжаем жить
            return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--project", required=True)
    args = parser.parse_args()
    win_ref_holder = {}
    def on_exc(receiver, exc, tb_str):
        win = win_ref_holder.get("win")
        if win is not None:
            win.handle_ui_exception(receiver, exc, tb_str)
    app = SafeApplication(sys.argv, on_exception=on_exc)
    # 1) Показать сплэш
    pix = QPixmap()  # можно подставить логотип: QPixmap(":/res/splash.png")
    splash = QSplashScreen(pix)
    splash.setWindowFlag(Qt.WindowType.WindowStaysOnTopHint)
    splash.show()
    splash.showMessage("Старт ManhwaStudio…", alignment=Qt.AlignmentFlag.AlignHCenter | Qt.AlignmentFlag.AlignBottom)
    app.processEvents()

    # 2) Создать окно быстро (минимально)
    win = AppWindow(args.project)
    win_ref_holder["win"] = win
    win.show()  # окно появится сразу (с пустыми вкладками)

    # 3) Запустить тяжёлую инициализацию «после» старта цикла событий
    def do_init():
        win.heavy_init(splash=splash, app=app)
        splash.finish(win)  # скрыть сплэш, когда всё готово

    QTimer.singleShot(0, do_init)

    sys.exit(app.exec())

if __name__ == "__main__":
    main()
