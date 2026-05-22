from __future__ import annotations

import inspect
from typing import Any


_SERVICE_REQUIRED_FIELDS: dict[str, tuple[str, ...]] = {
    "google": tuple(),
    "chatgpt": ("api_key",),
    "microsoft": ("api_key", "region"),
    "yandex": ("api_key",),
    "deepl": ("api_key",),
}


def _deep_translator_classes() -> dict[str, type[Any]]:
    from deep_translator import (  # type: ignore[import-not-found]
        ChatGptTranslator,
        DeeplTranslator,
        GoogleTranslator,
        MicrosoftTranslator,
        YandexTranslator,
    )

    return {
        "google": GoogleTranslator,
        "chatgpt": ChatGptTranslator,
        "microsoft": MicrosoftTranslator,
        "yandex": YandexTranslator,
        "deepl": DeeplTranslator,
    }


class MachineTranslationService:
    def health(self) -> dict[str, Any]:
        try:
            _deep_translator_classes()
        except Exception as exc:
            return {
                "available": False,
                "error": f"deep_translator is not available: {exc}",
            }
        return {"available": True}

    def translate_batch(
        self,
        *,
        service: str,
        source: str,
        target: str,
        params: dict[str, Any] | None,
        texts: list[str],
    ) -> list[dict[str, Any]]:
        service_key = str(service or "google").strip().lower() or "google"
        if not isinstance(texts, list) or not texts:
            raise ValueError("Field 'texts' must be a non-empty list.")

        classes = _deep_translator_classes()
        translator_cls = classes.get(service_key)
        if translator_cls is None:
            raise ValueError(f"Unknown translation service: {service_key}")

        kwargs: dict[str, Any] = {
            "source": str(source or "auto").strip() or "auto",
            "target": str(target or "ru").strip() or "ru",
        }
        if isinstance(params, dict):
            kwargs.update(params)

        missing = [
            key
            for key in _SERVICE_REQUIRED_FIELDS.get(service_key, tuple())
            if not str(kwargs.get(key, "") or "").strip()
        ]
        if missing:
            missing_csv = ", ".join(missing)
            raise ValueError(f"Missing required translator params: {missing_csv}")

        signature = inspect.signature(translator_cls.__init__)
        allowed = {name for name in signature.parameters if name != "self"}
        filtered_kwargs = {name: value for name, value in kwargs.items() if name in allowed}
        required_init_params = [
            name
            for name, meta in signature.parameters.items()
            if (
                name != "self"
                and meta.default is inspect._empty
                and meta.kind
                not in (
                    inspect.Parameter.VAR_POSITIONAL,
                    inspect.Parameter.VAR_KEYWORD,
                )
                and name not in filtered_kwargs
            )
        ]
        if required_init_params:
            missing_csv = ", ".join(required_init_params)
            raise ValueError(
                "Required translator constructor params are missing: " f"{missing_csv}"
            )

        try:
            translator = translator_cls(**filtered_kwargs)
        except Exception as exc:
            raise RuntimeError(f"Failed to initialize translator: {exc}") from exc

        results: list[dict[str, Any]] = []
        for text in texts:
            source_text = str(text or "")
            if not source_text.strip():
                results.append({"ok": True, "text": ""})
                continue
            try:
                translated = translator.translate(source_text)
            except Exception as exc:
                results.append({"ok": False, "error": str(exc)})
                continue
            results.append({"ok": True, "text": "" if translated is None else str(translated)})
        return results
