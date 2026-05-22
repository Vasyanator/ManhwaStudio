"""
Унифицированная система тем для ManhwaStudio.
Поддерживает светлую, серую и темную тему через QSS.
Разделяет цвет темы и стиль элементов (default/custom).
"""

from PyQt6.QtWidgets import QApplication
from PyQt6.QtGui import QPalette, QColor
from config import UserConfig
from typing import Literal


# ============== ЦВЕТОВЫЕ ПАЛИТРЫ ==============

# Темная тема (новая, более темная)
DARK_COLORS = {
    "bg_main": "#1a1a1a",
    "bg_window": "#121212",
    "bg_dialog": "#1a1a1a",
    "bg_widget": "#252525",
    "bg_input": "#313131",
    "bg_input_readonly": "#1e1e1e",
    "bg_button": "#2f2f2f",
    "bg_button_hover": "#3a3a3a",
    "bg_button_pressed": "#1f1f1f",
    "bg_button_disabled": "#222222",
    "bg_header": "#333333",
    "bg_list_hover": "#3a3a3a",
    "bg_scrollbar": "#1a1a1a",
    "bg_scrollbar_handle": "#454545",
    "bg_scrollbar_handle_hover": "#555555",
    "bg_tooltip": "#3a3a3a",
    "bg_menu": "#2a2a2a",
    "bg_tab": "#2f2f2f",
    "bg_tab_selected": "#1a1a1a",

    "text_main": "#d0d0d0",
    "text_disabled": "#555555",
    "text_tab": "#999999",
    "text_tab_selected": "#d0d0d0",
    "text_groupbox": "#999999",
    "text_statusbar": "#999999",

    "border_main": "#404040",
    "border_hover": "#505050",
    "border_focus": "#5c9cff",
    "border_disabled": "#333333",

    "selection_bg": "#5c9cff",
    "selection_text": "#ffffff",

    "accent": "#5c9cff",
    "accent_hover": "#7db3ff",

    "separator": "#404040",
    "gridline": "#404040",

    "arrow": "#888888",

    "btn_bg": "#2f2f2f",
    "btn_text": "#f0f0f0",
    "btn_border": "#3d3d3d",
    "btn_bg_hover": "#3c3c3c",
    "btn_border_hover": "#5a5a5a",
    "btn_bg_pressed": "#282828",
    "btn_bg_disabled": "#3a3a3a",

    "input_focus_border": "#6ba0ff",
    "input_bg_focus": "#2c2c2c",
    "input_bg_readonly": "#3b3b3b",

    "list_hover_bg": "#3a3a3a",

    "scroll_bg": "#2a2a2a",
    "scroll_handle": "#4a4a4a",
    "scroll_handle_hover": "#5d5d5d",

    "tab_bg": "#3a3a3a",
    "tab_selected_bg": "#2c2c2c",
    "tab_selected_text": "#ffffff",
    "tab_hover_bg": "#454545",

    "tooltip_bg": "#3b3b3b",

    "bg_statusbar": "#2b2b2b",
    "input_bg": "#1f1f1f",

}

# Серая тема (бывшая dark)
GRAY_COLORS = {
    "bg_main": "#2b2b2b",
    "bg_window": "#1e1e1e",
    "bg_dialog": "#2b2b2b",
    "bg_widget": "#3d3d3d",
    "bg_input": "#3d3d3d",
    "bg_input_readonly": "#383838",
    "bg_button": "#3d3d3d",
    "bg_button_hover": "#4a4a4a",
    "bg_button_pressed": "#2a2a2a",
    "bg_button_disabled": "#333333",
    "bg_header": "#4a4a4a",
    "bg_list_hover": "#4a4a4a",
    "bg_scrollbar": "#2b2b2b",
    "bg_scrollbar_handle": "#555555",
    "bg_scrollbar_handle_hover": "#666666",
    "bg_tooltip": "#4a4a4a",
    "bg_menu": "#3d3d3d",
    "bg_tab": "#3d3d3d",
    "bg_tab_selected": "#2b2b2b",

    "text_main": "#e0e0e0",
    "text_disabled": "#666666",
    "text_tab": "#b0b0b0",
    "text_tab_selected": "#e0e0e0",
    "text_groupbox": "#b0b0b0",
    "text_statusbar": "#b0b0b0",

    "border_main": "#555555",
    "border_hover": "#666666",
    "border_focus": "#5c9cff",
    "border_disabled": "#444444",

    "selection_bg": "#5c9cff",
    "selection_text": "#ffffff",

    "accent": "#5c9cff",
    "accent_hover": "#7db3ff",

    "separator": "#555555",
    "gridline": "#555555",

    "arrow": "#aaaaaa",

    "btn_bg": "#e0e0e0",
    "btn_text": "#1c1c1c",
    "btn_border": "#bcbcbc",
    "btn_bg_hover": "#e8e8e8",
    "btn_border_hover": "#9a9a9a",
    "btn_bg_pressed": "#d0d0d0",
    "btn_bg_disabled": "#dcdcdc",

    "input_focus_border": "#6ba0ff",
    "input_bg_focus": "#f2f2f2",
    "input_bg_readonly": "#d8d8d8",

    "list_hover_bg": "#e5e5e5",

    "scroll_bg": "#dcdcdc",
    "scroll_handle": "#b5b5b5",
    "scroll_handle_hover": "#999999",

    "tab_bg": "#d8d8d8",
    "tab_selected_bg": "#ededed",
    "tab_selected_text": "#1c1c1c",
    "tab_hover_bg": "#e3e3e3",

    "tooltip_bg": "#eaeaea",

    "bg_statusbar": "#e1e1e1",
    "input_bg": "#f2f2f2",

}

# Светлая тема
LIGHT_COLORS = {
    "bg_main": "#f5f5f5",
    "bg_window": "#ffffff",
    "bg_dialog": "#f5f5f5",
    "bg_widget": "#ffffff",
    "bg_input": "#ffffff",
    "bg_input_readonly": "#f8f8f8",
    "bg_button": "#e0e0e0",
    "bg_button_hover": "#d0d0d0",
    "bg_button_pressed": "#c0c0c0",
    "bg_button_disabled": "#e8e8e8",
    "bg_header": "#e8e8e8",
    "bg_list_hover": "#e8e8e8",
    "bg_scrollbar": "#f5f5f5",
    "bg_scrollbar_handle": "#cccccc",
    "bg_scrollbar_handle_hover": "#bbbbbb",
    "bg_tooltip": "#ffffcc",
    "bg_menu": "#ffffff",
    "bg_tab": "#e0e0e0",
    "bg_tab_selected": "#f5f5f5",

    "text_main": "#333333",
    "text_disabled": "#999999",
    "text_tab": "#666666",
    "text_tab_selected": "#333333",
    "text_groupbox": "#666666",
    "text_statusbar": "#666666",

    "border_main": "#cccccc",
    "border_hover": "#bbbbbb",
    "border_focus": "#3daee9",
    "border_disabled": "#dddddd",

    "selection_bg": "#3daee9",
    "selection_text": "#ffffff",

    "accent": "#3daee9",
    "accent_hover": "#5dbfff",

    "separator": "#cccccc",
    "gridline": "#dddddd",

    "arrow": "#666666",

    "btn_bg": "#f7f7f7",
    "btn_text": "#1a1a1a",
    "btn_border": "#cfcfcf",
    "btn_bg_hover": "#ffffff",
    "btn_border_hover": "#a8a8a8",
    "btn_bg_pressed": "#e6e6e6",
    "btn_bg_disabled": "#f0f0f0",

    "input_focus_border": "#4a90e2",
    "input_bg_focus": "#ffffff",
    "input_bg_readonly": "#eeeeee",

    "list_hover_bg": "#f1f1f1",

    "scroll_bg": "#f0f0f0",
    "scroll_handle": "#cccccc",
    "scroll_handle_hover": "#b3b3b3",

    "tab_bg": "#eaeaea",
    "tab_selected_bg": "#ffffff",
    "tab_selected_text": "#1a1a1a",
    "tab_hover_bg": "#f3f3f3",

    "tooltip_bg": "#fafafa",

    "bg_statusbar": "#f7f7f7",
    "input_bg": "#ffffff",
}

MAIN_QSS_DARK = """
QWidget {
    background: #242424;      /* Window */
    color: #fcfcfc;           /* WindowText/Text */
    selection-background-color: #3daee9; /* Highlight */
    selection-color: #fcfcfc; /* HighlightedText */
}

/* Base surfaces: inputs, text areas, views */
QLineEdit, QTextEdit, QPlainTextEdit, QSpinBox, QDoubleSpinBox,
QDateEdit, QTimeEdit, QDateTimeEdit, QComboBox,
QAbstractItemView, QListView, QTreeView, QTableView {
    background: #2b2b2b;      /* Base */
    color: #fcfcfc;           /* Text */
    selection-background-color: #3daee9;
    selection-color: #fcfcfc;
}

QAbstractItemView {
    alternate-background-color: #1d1f22; /* AlternateBase */
}

/* Buttons */
QPushButton, QToolButton, QCommandLinkButton {
    background: #2c2c2c;      /* Button */
    color: #fcfcfc;           /* ButtonText */
}

/* Tooltips */
QToolTip {
    background: #2c2c2c;      /* ToolTipBase */
    color: #fcfcfc;           /* ToolTipText */
}

/* Placeholder text */
QLineEdit, QTextEdit, QPlainTextEdit {
    placeholder-text-color: #f0f0f0; /* PlaceholderText */
}

/* Menus / popup lists */
QMenu, QMenuBar, QComboBox QAbstractItemView {
    background: #232323;      /* Window */
    color: #fcfcfc;
    selection-background-color: #3daee9;
    selection-color: #fcfcfc;
}

/* If you use rich text widgets, keep links white too */
QTextBrowser, QTextEdit {
    color: #fcfcfc;
}
QTextBrowser a, QTextEdit a {
    color: #fcfcfc;
    text-decoration: underline;
}
QTextBrowser a:visited, QTextEdit a:visited {
    color: #fcfcfc;
}

/* Scrollbars (color only; keeps default geometry) */
QScrollBar { background: #232323; }
QScrollBar::handle { background: #373b40; }  /* Mid */
QScrollBar::add-page, QScrollBar::sub-page { background: #232323; }
QScrollBar::handle:vertical {
    min-height: 30px;
}

/* Disabled */
*:disabled {
    color: #9aa0a6;
}

/* Focus (цвет рамки, без изменения толщины/формы) */
QLineEdit:focus, QTextEdit:focus, QPlainTextEdit:focus,
QComboBox:focus, QSpinBox:focus, QDoubleSpinBox:focus,
QDateEdit:focus, QTimeEdit:focus, QDateTimeEdit:focus,
QAbstractItemView:focus {
    border-color: #3daee9;
}

/* ToolBar / StatusBar */
QToolBar, QStatusBar {
    background: #232323;
    color: #fcfcfc;
}
QToolBar::separator {
    background: #373b40;
}

/* GroupBox */
QGroupBox {
    border-color: #373b40;
}
QGroupBox::title {
    color: #fcfcfc;
}

/* Headers (таблицы/деревья) */
QHeaderView::section {
    background: #232323;
    color: #fcfcfc;
    border-color: #373b40;
}
QTableCornerButton::section {
    background: #232323;
    border-color: #373b40;
}

/* ProgressBar */
QProgressBar {
    background: #2b2b2b;
    color: #fcfcfc;
    border-color: #373b40;
}
QProgressBar::chunk {
    background: #3daee9;
}

/* CheckBox / RadioButton (только цвет текста) */
QCheckBox, QRadioButton {
    color: #fcfcfc;
}

/* Sliders (только цвет) */
QSlider::groove:horizontal,
QSlider::groove:vertical {
    background: #373b40;
}
QSlider::handle:horizontal,
QSlider::handle:vertical {
    background: #3daee9;
}

/* Dock widgets */
QDockWidget {
    background: #242424;
    color: #fcfcfc;
}
QDockWidget::title {
    background: #232323;
    color: #fcfcfc;
}

/* MessageBox / Dialogs */
QDialog, QMessageBox {
    background: #242424;
    color: #fcfcfc;
}



QScrollArea, QStackedWidget, QFrame {
    background: #242424;
    border: none;
}

/* Обязательно для таблиц и списков, чтобы углы не были белыми */
QAbstractScrollArea::corner {
    background: #242424;
    border: none;
}

QToolTip {
    background-color: #2c2c2c;
    color: #fcfcfc;
    border: 1px solid #3d3d3d; /* Важно добавить границу */
}
"""

def _get_colors_for_theme(theme_name: str) -> dict:
    """Возвращает цветовую палитру для указанной темы."""
    if theme_name == "light":
        return LIGHT_COLORS
    elif theme_name == "gray":
        return GRAY_COLORS
    else:  # dark
        return DARK_COLORS


def get_theme_qss(theme_name: str = None, style_name: str = None) -> str:
    """
    Возвращает QSS для указанной темы и стиля.

    Args:
        theme_name: "dark", "gray" или "light". Если None, читает из UserConfig.
        style_name: "modern" или "default". Если None, читает из UserConfig.

    Returns:
        Строка с QSS стилями (пустая для default стиля).
    """
    if theme_name is None:
        try:
            theme_name = UserConfig.General.theme
        except Exception:
            theme_name = "dark"

    if style_name is None:
        try:
            style_name = UserConfig.General.style
        except Exception:
            style_name = "default"

    # Если стиль default, не применяем кастомный QSS


    # Получаем цвета для темы
    # elif style_name == "glass_light":
    #     return GLASS_LIGHT_STYLE_TEMPLATE.format(**colors)
    # Применяем цвета к шаблону
    
    return ""


def apply_theme(app_or_widget=None, theme_name: str = None, style_name: str = None):
    """
    Применяет тему к приложению или виджету.

    Args:
        app_or_widget: QApplication или QWidget. Если None, применяет к текущему QApplication.
        theme_name: "dark", "gray" или "light". Если None, читает из UserConfig.
        style_name: "modern" или "default". Если None, читает из UserConfig.
    """
    qss = get_theme_qss(theme_name, style_name)

    if app_or_widget is None:
        app_or_widget = QApplication.instance()

    if app_or_widget is not None:
        app_or_widget.setStyleSheet(qss)


def get_current_theme() -> str:
    """
    Возвращает текущую тему из настроек.

    Returns:
        "dark", "gray" или "light"
    """
    try:
        theme = UserConfig.General.theme
        if theme in ("dark", "gray", "light"):
            return theme
    except Exception:
        pass
    return "dark"


def get_current_style() -> str:
    """
    Возвращает текущий стиль из настроек.

    Returns:
        "modern" или "default"
    """
    try:
        style = UserConfig.General.style
        if style in ("modern", "default"):
            return style
    except Exception:
        pass
    return "modern"


def set_theme(theme_name: str, style_name: str = None):
    """
    Устанавливает тему в настройках и применяет её.

    Args:
        theme_name: "dark", "gray" или "light"
        style_name: "modern" или "default". Если None, не меняет текущий стиль.
    """
    if theme_name not in ("dark", "gray", "light"):
        raise ValueError(f"Invalid theme name: {theme_name}. Must be 'dark', 'gray' or 'light'.")

    UserConfig.General.theme = theme_name

    if style_name is not None:
        if style_name not in ("modern", "default"):
            raise ValueError(f"Invalid style name: {style_name}. Must be 'modern' or 'default'.")
        UserConfig.General.style = style_name

    apply_theme(theme_name=theme_name, style_name=style_name)


def set_style(style_name: str):
    """
    Устанавливает стиль в настройках и применяет его.

    Args:
        style_name: "modern" или "default"
    """
    if style_name not in ("modern", "default"):
        raise ValueError(f"Invalid style name: {style_name}. Must be 'modern' or 'default'.")

    UserConfig.General.style = style_name
    apply_theme(style_name=style_name)


def get_canvas_background_color(theme_name: str = None) -> str:
    """
    Возвращает цвет фона холста (чуть темнее основного фона).

    Args:
        theme_name: "dark", "gray" или "light". Если None, читает из UserConfig.

    Returns:
        Hex-цвет для фона холста.
    """
    if theme_name is None:
        theme_name = get_current_theme()

    # Холст должен быть темнее основного фона
    if theme_name == "light":
        return "#e0e0e0"
    elif theme_name == "gray":
        return "#1a1a1a"
    else:  # dark
        return "#0a0a0a"
Theme = Literal["dark", "gray", "light"]

def make_palette(theme: Theme) -> QPalette:
    pal = QPalette()
    if theme == "light":
        # Светлая палитра
        pal.setColor(QPalette.ColorRole.Window, QColor(245, 245, 247))
        pal.setColor(QPalette.ColorRole.Base, QColor(255, 255, 255))
        pal.setColor(QPalette.ColorRole.AlternateBase, QColor(245, 245, 247))
        pal.setColor(QPalette.ColorRole.Text, QColor(20, 20, 22))
        pal.setColor(QPalette.ColorRole.Button, QColor(248, 248, 250))
        pal.setColor(QPalette.ColorRole.ButtonText, QColor(20, 20, 22))
        pal.setColor(QPalette.ColorRole.ToolTipBase, QColor(255, 255, 255))
        pal.setColor(QPalette.ColorRole.ToolTipText, QColor(20, 20, 22))
        pal.setColor(QPalette.ColorRole.Highlight, QColor(38, 132, 255))
        pal.setColor(QPalette.ColorRole.HighlightedText, QColor(255, 255, 255))
        pal.setColor(QPalette.ColorRole.WindowText, QColor(20, 20, 22))
        pal.setColor(QPalette.ColorRole.PlaceholderText, QColor(0, 0, 0, 120))
    elif theme == "gray":
        # Серая палитра (менее тёмная)
        pal.setColor(QPalette.ColorRole.Window, QColor(43, 43, 43))
        pal.setColor(QPalette.ColorRole.Base, QColor(61, 61, 61))
        pal.setColor(QPalette.ColorRole.AlternateBase, QColor(74, 74, 74))
        pal.setColor(QPalette.ColorRole.Text, QColor(224, 224, 224))
        pal.setColor(QPalette.ColorRole.Button, QColor(61, 61, 61))
        pal.setColor(QPalette.ColorRole.ButtonText, QColor(224, 224, 224))
        pal.setColor(QPalette.ColorRole.ToolTipBase, QColor(74, 74, 74))
        pal.setColor(QPalette.ColorRole.ToolTipText, QColor(224, 224, 224))
        pal.setColor(QPalette.ColorRole.Highlight, QColor(92, 156, 255))
        pal.setColor(QPalette.ColorRole.HighlightedText, QColor(255, 255, 255))
        pal.setColor(QPalette.ColorRole.WindowText, QColor(224, 224, 224))
        pal.setColor(QPalette.ColorRole.PlaceholderText, QColor(255, 255, 255, 120))
    else:
        # Тёмная палитра (как было, чуть унифицировал)
        pal.setColor(QPalette.ColorRole.Window, QColor(12, 12, 14))
        pal.setColor(QPalette.ColorRole.Base, QColor(18, 18, 20))
        pal.setColor(QPalette.ColorRole.AlternateBase, QColor(24, 24, 28))
        pal.setColor(QPalette.ColorRole.Text, QColor(235, 235, 235))
        pal.setColor(QPalette.ColorRole.Button, QColor(26, 26, 30))
        pal.setColor(QPalette.ColorRole.ButtonText, QColor(235, 235, 235))
        pal.setColor(QPalette.ColorRole.ToolTipBase, QColor(24, 24, 28))
        pal.setColor(QPalette.ColorRole.ToolTipText, QColor(235, 235, 235))
        pal.setColor(QPalette.ColorRole.Highlight, QColor(70, 120, 255))
        pal.setColor(QPalette.ColorRole.HighlightedText, QColor(255, 255, 255))
        pal.setColor(QPalette.ColorRole.WindowText, QColor(235, 235, 235))
        pal.setColor(QPalette.ColorRole.PlaceholderText, QColor(255, 255, 255, 120))