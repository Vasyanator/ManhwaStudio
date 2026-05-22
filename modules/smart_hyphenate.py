import pyphen

HARD_HYPHEN = "-"
SOFT_HYPHEN = "\u00AD" 

def _break_at(word: str, max_prefix_len: int, dic: pyphen.Pyphen, *, hyphen_char: str = "-") -> str:
    n = len(word)
    if n == 0:
        return word

    max_left = max(1, min(n - 1, max_prefix_len - 1))
    positions = dic.positions(word)  # индексы после которых можно переносить
    p = max((pos for pos in positions if pos <= max_left), default=None)
    if p is None:
        p = max_left

    return word[:p] + hyphen_char + word[p:]


def smart_hyphenate(
    word: str,
    *,
    fit: int | None = None,
    overflow: int | None = None,
    lang: str = "ru_RU",
    all_positions: bool = False,
    hyphen_char: str | None = None,   # чем помечать (по умолчанию мягкий перенос)
) -> str:
    """
    Режимы:
      - all_positions=True: отметить ВСЕ допустимые точки переноса (игнорирует fit/overflow).
      - fit:  максимальная длина первой строки (включая символ переноса).
      - overflow: сколько символов с конца не влезает.

    По умолчанию для all_positions используется мягкий перенос U+00AD,
    а для одиночного разрыва — обычный дефис "-".
    """
    if not word:
        return word

    dic = pyphen.Pyphen(lang=lang)

    # --- Режим: отметить все позиции переноса ---
    if all_positions:
        mark = hyphen_char if hyphen_char is not None else SOFT_HYPHEN
        pos = set(dic.positions(word))
        if not pos:
            return word
        # positions — индексы после которых можно переносить (1..len-1)
        out = []
        for i, ch in enumerate(word, 1):
            out.append(ch)
            if i in pos:
                out.append(mark)
        return "".join(out)

    # --- Обычные режимы: fit / overflow ---
    if (fit is None) == (overflow is None):
        raise ValueError("Нужно указать ровно один параметр: либо fit, либо overflow.")

    if overflow is not None:
        n = len(word)
        if overflow <= 0:
            return word
        max_prefix_len = max(2, min(n, n - overflow))  # буква+дефис минимум
        return _break_at(word, max_prefix_len, dic, hyphen_char="-")

    # fit-ветка
    if fit <= 1:
        # «буква-дефис» не влезает — переносим перед последней
        return word[:-1] + "-" + word[-1:]
    return _break_at(word, fit, dic, hyphen_char="-")



# --- быстрые проверки ---
if __name__ == "__main__":
    print(smart_hyphenate("гиперболизация", fit=7))       # гипер-болизация
    print(smart_hyphenate("гиперболизация", overflow=5))  # гипер-болизация
    print(smart_hyphenate("непрерывность", fit=6))        # непре-рывность (пример)
