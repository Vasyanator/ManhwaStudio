import tkinter as tk
from modules.bind_patch import patch_bind_layout_agnostic
patch_bind_layout_agnostic()
import os
import config #нужно для инициализации
import traceback
import sys, os
from modules.open_project_window import open_project
from modules.new_project_window import show_new_project_full as show_new_project

from modules.win_lin_support import zoomed
# >>> model manager: импорт заглушки
from modules.model_manager import ModelManager

VERSION = "2.0"

def ensure_mapped(widget):
    """Дождаться, пока виджет реально отобразится (map), чтобы визуал/depth были валидными."""
    widget.update_idletasks()
    try:
        widget.wait_visibility()
    except Exception:
        # На некоторых WM wait_visibility может не сработать — форсируем циклом обновления
        for _ in range(5):
            widget.update()
            widget.update_idletasks()

def handoff_to_qt(project_path: str):
    """
    Полностью закрывает Tk и заменяет процесс на PyQt-приложение.
    """
    try:
        # 1) Корректно уничтожаем все Tk-окна
        for w in list(tk._default_root.children.values()):
            try:
                w.destroy()
            except Exception:
                pass
        if tk._default_root:
            tk._default_root.destroy()
    except Exception:
        pass

    # 2) Запускаем (желательно — os.execv), чтобы не оставлять следов Tk
    py = sys.executable
    qt_runner = os.path.join(os.path.dirname(__file__), "qt_runner.py")
    os.execv(py, [py, qt_runner, "--project", project_path])

def safe_zoomed(win):
    """Твоя zoomed(), но вызываем ТОЛЬКО после ensure_mapped(win)."""
    zoomed(win)  # оставляем твою реализацию

def main():
    root = tk.Tk()
    root.withdraw()

    root.deiconify()
    ensure_mapped(root)
    safe_zoomed(root)

    root.title(f'ManhwaStudio v{VERSION}: Выбор проекта')

    # ---------- верхний заголовок ----------
    label_top = tk.Label(root, text="ManhwaStudio", font=("Arial", 24))
    label_top.pack(side="top", anchor="n")

    # ---------- зона с тремя панелями ----------
    content = tk.Frame(root, bd=0, highlightthickness=0)
    content.pack(side="top", fill="both", expand=True, padx=8, pady=8)

    # сетка 1x3 без видимых границ
    content.grid_rowconfigure(0, weight=1)
    for col in range(3):
        content.grid_columnconfigure(col, weight=1, uniform="cols")

    # левая панель (пока пустая)
    left_panel = tk.Frame(content, bd=0, highlightthickness=0)
    left_panel.grid(row=0, column=0, sticky="nsew")

    # центральная панель — кнопки проекта вверху
    center_panel = tk.Frame(content, bd=0, highlightthickness=0)
    center_panel.grid(row=0, column=1, sticky="nsew")

    # контейнер, «прилипший» к верху
    center_top = tk.Frame(center_panel, bd=0, highlightthickness=0)
    center_top.pack(side="top", anchor="n", pady=4)

    tk.Button(center_top, text='Открыть проект', width=20,
              command=lambda: open_project(root, qt=True)).pack(pady=6, padx=20)
    tk.Button(center_top, text='Новый проект', width=20,
              command=lambda: show_new_project(root, qt=True)).pack(pady=6, padx=20)

    # правая панель — менеджер моделей по центру,
    # индикатор НИЖЕ кнопки
    right_panel = tk.Frame(content, bd=0, highlightthickness=0)
    right_panel.grid(row=0, column=2, sticky="nsew")

    right_center = tk.Frame(right_panel, bd=0, highlightthickness=0)
    right_center.pack(expand=True)  # центрируем по вертикали

    # инициализируем заглушку менеджера
    model_manager = ModelManager()

    # кнопка «Менеджер моделей»
    tk.Button(
        right_center,
        text="Менеджер моделей",
        width=20,
        command=lambda: model_manager.open_manager(root)
    ).pack(pady=(0, 6), padx=20)

    # индикатор (кружок + подпись) — ниже кнопки
    indicator_frame = tk.Frame(right_center, bd=0, highlightthickness=0)
    indicator_frame.pack(pady=(0, 6))

    indicator_canvas = tk.Canvas(indicator_frame, width=14, height=14, highlightthickness=0, bd=0)
    indicator_canvas.pack(side="left")
    indicator_circle = indicator_canvas.create_oval(2, 2, 12, 12, fill="#9e9e9e", outline="#9e9e9e")

    indicator_label = tk.Label(indicator_frame, text="Не запущен", font=("Arial", 10))
    indicator_label.pack(side="left", padx=6)

    def render_indicator(state: dict):
        color = state.get("color", "#9e9e9e")
        text = state.get("text", "—")
        indicator_canvas.itemconfig(indicator_circle, fill=color, outline=color)
        indicator_label.config(text=text)

    model_manager.on_status_change = render_indicator
    render_indicator(model_manager.get_status())

    # ---------- низ ----------
    label_bottom = tk.Label(root, text="Разработал ChatGPT под руководством Vasyanator", font=("Arial", 14))
    label_bottom.pack(side="bottom", anchor="s")

    root.mainloop()


if __name__ == '__main__':
    main()
