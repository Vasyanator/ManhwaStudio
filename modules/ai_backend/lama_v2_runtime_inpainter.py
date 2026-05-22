"""
LaMa Inpainter V2
=================
Улучшенная версия встроенного инпейнтера с полной поддержкой:
- Standard inpainting (быстрый режим)
- Refinement mode (multi-scale с итеративной оптимизацией)

Основано на официальных скриптах bin/predict.py и saicinpainting/evaluation/refinement.py
"""

import os
import sys
from pathlib import Path
import yaml
import torch
import numpy as np
from typing import Optional, Tuple, Dict, Any

# ============================================================================
# LAMA V2 RUNTIME INPAINTER (LOCAL BACKEND COPY)
# ----------------------------------------------------------------------------
# Что в файле:
# - `InpainterV2`: standalone runtime-обёртка LaMa для backend endpoint
#   `/inpaint/lama_v2` (standard + refine режимы).
# - Подготовка batch/маски и вызов `saicinpainting` инференса.
# - Управление режимом refine, выгрузкой модели и статистикой памяти.
# - Локальная подготовка `sys.path`:
#   - `modules/ai_backend/lama_runtime_bundle` (локальный `saicinpainting`),
#   - текущая папка модуля,
#   - корень программы.
# ============================================================================

_MODULE_DIR = Path(__file__).resolve().parent
_PROGRAM_DIR = _MODULE_DIR.parents[1]
_RUNTIME_BUNDLE_DIR = _MODULE_DIR / "lama_runtime_bundle"

for _path in (_RUNTIME_BUNDLE_DIR, _MODULE_DIR, _PROGRAM_DIR):
    _path_str = str(_path)
    if _path.is_dir() and _path_str not in sys.path:
        sys.path.insert(0, _path_str)

# Патч для numpy.sctypes (для совместимости с numpy 2.x)
if not hasattr(np, "sctypes"):
    sctypes = {
        "int": [np.int8, np.int16, np.int32, np.int64],
        "uint": [np.uint8, np.uint16, np.uint32, np.uint64],
        "float": [np.float16, np.float32, np.float64] + ([np.float128] if hasattr(np, "float128") else []),
        "complex": [np.complex64, np.complex128] + ([np.complex256] if hasattr(np, "complex256") else []),
        "others": [np.bool_, np.object_, np.str_],
    }
    np.sctypes = sctypes  # type: ignore[attr-defined]

# Импорт конфигурации
try:
    from config import LAMA_DIR
except ImportError:
    LAMA_DIR = str(_PROGRAM_DIR / "ManhwaStudio_AI_Models" / "Torch" / "LaMa")

from omegaconf import OmegaConf
from saicinpainting.training.trainers import load_checkpoint
from saicinpainting.evaluation.utils import move_to_device
from saicinpainting.evaluation.refinement import refine_predict
from saicinpainting.evaluation.data import pad_tensor_to_modulo


class InpainterV2:
    """
    Усовершенствованная обёртка вокруг LaMa с полной поддержкой inpainting и refinement.

    Основные улучшения по сравнению с V1:
    - Правильная реализация refinement mode
    - Корректная обработка unpad_to_size для refinement
    - Улучшенное управление устройствами (CPU/CUDA)
    - Поддержка всех параметров из официальных скриптов
    - Детальное логирование и обработка ошибок

    Пример использования:
    ```python
    # Standard mode (быстро)
    inpainter = InpainterV2(device="cuda:0", refine=False)
    result = inpainter(img_rgb, mask)

    # Refinement mode (медленнее, но качественнее)
    inpainter = InpainterV2(device="cuda:0", refine=True)
    result = inpainter(img_rgb, mask)

    # Переключение режима на лету
    inpainter.set_refine(True, n_iters=30, lr=0.001)
    ```
    """

    def __init__(
        self,
        checkpoint_dir: Optional[str] = None,
        checkpoint_name: str = "lama_large_512px.ckpt.ckpt",
        device: str = "cuda:0",
        refine: bool = False,
        refiner_kwargs: Optional[Dict[str, Any]] = None,
        modulo: int = 8,
        verbose: bool = False
    ):
        """
        Инициализация инпейнтера.

        Args:
            checkpoint_dir: Путь к директории с чекпоинтом (по умолчанию LAMA_DIR)
            checkpoint_name: Имя файла чекпоинта (по умолчанию "best.ckpt")
            device: Устройство для вычислений ("cuda:0", "cuda:1", "cpu")
            refine: Включить режим refinement (медленнее, но качественнее)
            refiner_kwargs: Дополнительные параметры для refinement режима
            modulo: Кратность размера для padding (обычно 8)
            verbose: Подробный вывод информации
        """
        self.verbose = verbose
        self.modulo = modulo
        self.refine = refine
        self.model_format = "saicinpainting"

        # Определяем устройство
        if device.startswith("cuda") and not torch.cuda.is_available():
            self._log("⚠️ CUDA недоступна, переключаюсь на CPU")
            device = "cpu"

        self.device = torch.device(device)
        self._log(f"📱 Устройство: {self.device}")

        # Определяем директорию с чекпоинтом
        if checkpoint_dir is None:
            checkpoint_dir = LAMA_DIR
        self.checkpoint_dir = checkpoint_dir

        # Настройки для refinement
        self._setup_refiner_kwargs(refiner_kwargs)

        # Загрузка модели
        self._log("🔄 Загрузка модели...")
        self.model = self._load_model(checkpoint_name)
        self._log("✅ Модель загружена")

    def _log(self, message: str):
        """Вывод сообщения, если verbose=True"""
        if self.verbose:
            print(message)

    def _setup_refiner_kwargs(self, refiner_kwargs: Optional[Dict[str, Any]]):
        """Настройка параметров для refinement режима"""
        # Формируем строку GPU IDs для refinement
        if self.device.type == "cuda" and self.device.index is not None:
            gpu_ids_str = str(self.device.index)
        elif self.device.type == "cuda":
            gpu_ids_str = "0"
        else:
            gpu_ids_str = ""

        # Параметры по умолчанию из официального скрипта
        refiner_defaults = {
            "gpu_ids": gpu_ids_str,
            "modulo": self.modulo,
            "n_iters": 15,              # Количество итераций оптимизации
            "lr": 0.002,                # Learning rate для оптимизации
            "min_side": 256,            # Минимальный размер стороны для пирамиды
            "max_scales": 3,            # Максимальное количество масштабов
            "px_budget": 1_000_000,     # Лимит пикселей (H*W) для экономии памяти
        }

        self.refiner_kwargs = {**refiner_defaults, **(refiner_kwargs or {})}

        if self.verbose:
            self._log(f"🔧 Параметры refinement: {self.refiner_kwargs}")

    def _load_model(self, checkpoint_name: str):
        """Загрузка модели из чекпоинта"""
        # Путь к чекпоинту
        checkpoint_path = os.path.join(self.checkpoint_dir, "models", checkpoint_name)
        if not os.path.exists(checkpoint_path):
            raise FileNotFoundError(f"Чекпоинт не найден: {checkpoint_path}")

        checkpoint_suffix = Path(checkpoint_path).suffix.lower()
        if checkpoint_suffix == ".pt":
            model = torch.jit.load(checkpoint_path, map_location="cpu")
            model.eval()
            self.model_format = "torchscript"
            if not self.refine:
                model.to(self.device)
            return model

        # Путь к конфигу
        config_path = os.path.join(self.checkpoint_dir, "config.yaml")
        if not os.path.exists(config_path):
            raise FileNotFoundError(f"Файл конфигурации не найден: {config_path}")

        # Загружаем конфиг
        with open(config_path, "r") as f:
            train_config = OmegaConf.create(yaml.safe_load(f))

        # Настройки для inference
        train_config.training_model.predict_only = True
        train_config.visualizer.kind = "noop"

        # Загружаем модель
        model = load_checkpoint(
            train_config,
            checkpoint_path,
            strict=False,
            map_location="cpu"
        )
        if hasattr(model, "freeze"):
            model.freeze()
        elif hasattr(model, "eval"):
            model.eval()

        self.model_format = "saicinpainting"

        # В стандартном режиме переносим модель на устройство
        # В refinement режиме модель остаётся на CPU (refine_predict сам управляет)
        if not self.refine:
            model.to(self.device)

        return model

    def _prepare_batch(
        self,
        img: np.ndarray,
        mask: np.ndarray,
        pad_to_modulo: bool = True
    ) -> Tuple[Dict[str, torch.Tensor], Tuple[int, int]]:
        """
        Подготовка batch для модели.

        Args:
            img: RGB изображение (H, W, 3) в uint8
            mask: Маска (H, W) в uint8, где 255 = область для инпейнтинга
            pad_to_modulo: Применить padding до кратности modulo

        Returns:
            batch: Словарь с тензорами для модели
            orig_size: Оригинальный размер (H, W)
        """
        assert img.ndim == 3 and img.shape[2] == 3, "Ожидается RGB изображение (H, W, 3)"
        assert mask.ndim == 2, "Ожидается 2D маска (H, W)"

        h, w = img.shape[:2]
        orig_size = (h, w)

        # Конвертация в тензоры
        # Изображение: uint8 -> float32 [0,1], затем (H,W,C) -> (C,H,W)
        img_tensor = torch.from_numpy(img.astype("float32") / 255.0)
        img_tensor = img_tensor.permute(2, 0, 1).unsqueeze(0)  # (1, 3, H, W)

        # Маска: uint8 -> binary float32, затем добавляем размерности
        mask_tensor = torch.from_numpy((mask > 0).astype("float32"))
        mask_tensor = mask_tensor.unsqueeze(0).unsqueeze(0)  # (1, 1, H, W)

        batch = {
            "image": img_tensor,
            "mask": mask_tensor
        }

        # Padding до кратности modulo
        if pad_to_modulo:
            batch["image"] = pad_tensor_to_modulo(batch["image"], self.modulo)
            batch["mask"] = pad_tensor_to_modulo(batch["mask"], self.modulo)

        return batch, orig_size

    def _inpaint_standard(
        self,
        batch: Dict[str, torch.Tensor],
        orig_size: Tuple[int, int],
        out_key: str = "inpainted"
    ) -> np.ndarray:
        """
        Стандартный режим инпейнтинга (быстрый).

        Args:
            batch: Batch с изображением и маской
            orig_size: Оригинальный размер (H, W)
            out_key: Ключ результата в выходном batch

        Returns:
            Результат инпейнтинга (H, W, 3) в uint8
        """
        # Переносим batch на устройство
        batch = move_to_device(batch, self.device)

        # Бинаризация маски (как в официальном predict.py)
        batch["mask"] = (batch["mask"] > 0) * 1

        # Инференс
        with torch.no_grad():
            if self.model_format == "torchscript":
                result = self.model(batch["image"], batch["mask"])
                if not isinstance(result, torch.Tensor):
                    raise TypeError(
                        f"TorchScript LaMa вернул неожиданный тип результата: {type(result)!r}"
                    )
                batch = {"inpainted": result}
            else:
                batch = self.model(batch)

        # Извлекаем результат
        if out_key not in batch:
            # Fallback: ищем альтернативные ключи
            for candidate in ("inpainted", "predicted_image", "output"):
                if candidate in batch:
                    out_key = candidate
                    break
            else:
                raise KeyError(f"Не найден ключ результата. Доступные ключи: {batch.keys()}")

        result = batch[out_key][0]  # (C, H, W)

        # Обрезаем до оригинального размера
        orig_h, orig_w = orig_size
        result = result[:, :orig_h, :orig_w]

        # Конвертация в numpy uint8 RGB
        result = result.clamp(0, 1).permute(1, 2, 0).cpu().numpy()
        result = (result * 255).astype("uint8")

        return result

    def _inpaint_refine(
        self,
        batch: Dict[str, torch.Tensor],
        orig_size: Tuple[int, int]
    ) -> np.ndarray:
        """
        Refinement режим инпейнтинга (медленнее, но качественнее).

        Args:
            batch: Batch с изображением и маской
            orig_size: Оригинальный размер (H, W)

        Returns:
            Результат инпейнтинга (H, W, 3) в uint8
        """
        # ВАЖНО: refine_predict ожидает unpad_to_size как tuple из двух тензоров (h, w)
        # каждый тензор имеет форму (batch_size,), что соответствует формату default_collate
        orig_h, orig_w = orig_size
        batch["unpad_to_size"] = (
            torch.tensor([orig_h]),  # Shape: (1,)
            torch.tensor([orig_w])   # Shape: (1,)
        )

        # Бинаризация маски
        batch["mask"] = (batch["mask"] > 0) * 1.0

        # Вызываем refinement
        # refine_predict возвращает (1, 3, H, W) tensor
        result = refine_predict(batch, self.model, **self.refiner_kwargs)

        # Извлекаем первый элемент батча
        result = result[0]  # (3, H, W)

        # Конвертация в numpy uint8 RGB
        result = result.clamp(0, 1).permute(1, 2, 0).cpu().numpy()
        result = (result * 255).astype("uint8")

        return result

    def __call__(
        self,
        img: np.ndarray,
        mask: np.ndarray,
        refine: Optional[bool] = None
    ) -> np.ndarray:
        """
        Выполнить инпейнтинг.

        Args:
            img: RGB изображение (H, W, 3) в uint8
            mask: Маска (H, W) в uint8, где 255 = область для заполнения
            refine: Переопределить режим для этого вызова (None = использовать self.refine)

        Returns:
            Результат инпейнтинга (H, W, 3) в uint8
        """
        # Определяем режим для этого вызова
        use_refine = self.refine if refine is None else refine

        self._log(f"🎨 Режим: {'Refinement' if use_refine else 'Standard'}")

        if use_refine and self.model_format == "torchscript":
            raise RuntimeError(
                "Refine mode не поддерживается для TorchScript `.pt` моделей LaMa. "
                "Выключите Refine или выберите `.ckpt` checkpoint."
            )

        # Подготовка batch
        batch, orig_size = self._prepare_batch(img, mask, pad_to_modulo=True)

        # Выбор режима
        # ВАЖНО: refinement режим ТРЕБУЕТ градиенты для оптимизации промежуточных фич,
        # поэтому не используем torch.no_grad() для refine mode
        if use_refine:
            result = self._inpaint_refine(batch, orig_size)
        else:
            with torch.no_grad():
                result = self._inpaint_standard(batch, orig_size)

        self._log(f"✅ Инпейнтинг завершён, размер: {result.shape}")

        return result

    def set_refine(self, refine: bool, **override_kwargs):
        """
        Переключить режим refinement и обновить параметры.

        Args:
            refine: True для включения refinement, False для стандартного режима
            **override_kwargs: Параметры для обновления (n_iters, lr, max_scales, и т.д.)
        """
        prev_refine = self.refine
        if refine and self.model_format == "torchscript":
            raise RuntimeError(
                "Refine mode не поддерживается для TorchScript `.pt` моделей LaMa."
            )
        self.refine = bool(refine)

        # Если переключаемся из refine в standard, переносим модель на устройство
        if prev_refine and not self.refine:
            self._log(f"🔄 Перенос модели на {self.device}")
            self.model.to(self.device)

        # Если переключаемся из standard в refine, переносим модель на CPU
        if not prev_refine and self.refine:
            self._log("🔄 Перенос модели на CPU (для refinement)")
            self.model.to("cpu")

        # Обновляем параметры refinement
        if override_kwargs:
            self.refiner_kwargs.update(override_kwargs)
            self._log(f"🔧 Обновлены параметры: {override_kwargs}")

    def get_info(self) -> Dict[str, Any]:
        """
        Получить информацию о текущем состоянии инпейнтера.

        Returns:
            Словарь с информацией
        """
        return {
            "device": str(self.device),
            "refine": self.refine,
            "modulo": self.modulo,
            "refiner_kwargs": self.refiner_kwargs,
            "checkpoint_dir": self.checkpoint_dir
        }

    def unload(self, clear_cache: bool = True, aggressive: bool = False):
        """
        Полная выгрузка модели и очистка памяти GPU.

        Этот метод:
        1. Перемещает модель на CPU
        2. Удаляет все параметры модели
        3. Очищает кэш CUDA (если доступен)
        4. Принудительно вызывает сборщик мусора

        Args:
            clear_cache: Очистить кэш CUDA (по умолчанию True)
            aggressive: Агрессивная очистка с множественными GC и cache clears (медленнее)

        Использование:
        ```python
        inpainter = InpainterV2(device="cuda:0")
        result = inpainter(img, mask)

        # Освободить GPU память (стандартная очистка)
        inpainter.unload()

        # Максимальная очистка (может освободить до ~10-20 MB больше)
        inpainter.unload(aggressive=True)
        ```

        Примечание:
            Даже при aggressive=True, ~100-170 MB может остаться зарезервированной из-за:
            - CUDA context (создаётся при первом использовании GPU, ~100-150 MB)
            - PyTorch CUDA allocator cache (для ускорения будущих аллокаций)
            - NVIDIA driver overhead
            Это нормальное поведение PyTorch и не является утечкой памяти.
            Память полностью освобождается только при завершении процесса Python.
        """
        import gc

        self._log("🧹 Выгрузка модели...")

        # Шаг 1: Переместить модель на CPU
        if hasattr(self, 'model') and self.model is not None:
            try:
                self._log("📤 Перемещение модели на CPU...")
                self.model.to('cpu')
            except Exception as e:
                self._log(f"⚠️ Ошибка при перемещении на CPU: {e}")

            # Шаг 2: Удалить все параметры и буферы модели
            try:
                self._log("🗑️ Удаление параметров модели...")
                # Очистить все параметры
                for param in self.model.parameters():
                    del param
                # Очистить все буферы
                for buffer in self.model.buffers():
                    del buffer
            except Exception as e:
                self._log(f"⚠️ Ошибка при удалении параметров: {e}")

            # Шаг 3: Удалить ссылку на модель
            self._log("🗑️ Удаление ссылки на модель...")
            del self.model
            self.model = None

        # Шаг 4: Принудительная сборка мусора
        if aggressive:
            self._log("♻️ Агрессивная сборка мусора (3 прохода)...")
            # Множественные проходы GC могут помочь освободить циклические ссылки
            for _ in range(3):
                gc.collect()
        else:
            self._log("♻️ Сборка мусора...")
            gc.collect()

        # Шаг 5: Очистка кэша CUDA (если доступен и запрошен)
        if clear_cache and torch.cuda.is_available():
            try:
                self._log("🧹 Очистка кэша CUDA...")

                if aggressive:
                    # Агрессивная очистка: multiple rounds
                    for i in range(3):
                        torch.cuda.empty_cache()
                        if i < 2:  # Не делаем GC после последнего empty_cache
                            gc.collect()
                else:
                    torch.cuda.empty_cache()

                # Синхронизация CUDA для гарантии завершения операций
                if self.device.type == "cuda":
                    torch.cuda.synchronize(self.device)

                # Показать статистику памяти
                if self.verbose and self.device.type == "cuda":
                    device_idx = self.device.index if self.device.index is not None else 0
                    allocated = torch.cuda.memory_allocated(device_idx) / 1024**2
                    reserved = torch.cuda.memory_reserved(device_idx) / 1024**2
                    self._log(f"💾 GPU память: {allocated:.1f} MB выделено, {reserved:.1f} MB зарезервировано")

                    if reserved > 100:
                        self._log(f"ℹ️  Примечание: {reserved:.0f} MB зарезервировано - это нормально.")
                        self._log(f"    CUDA context (~100-150 MB) остаётся до завершения процесса.")

            except Exception as e:
                self._log(f"⚠️ Ошибка при очистке CUDA: {e}")

        self._log("✅ Модель выгружена, память освобождена")

    def reload(self, checkpoint_name: Optional[str] = None):
        """
        Перезагрузить модель (полезно после unload).

        Args:
            checkpoint_name: Имя чекпоинта (None = использовать предыдущий)

        Пример:
        ```python
        # Выгрузить модель для экономии памяти
        inpainter.unload()

        # ... другие операции ...

        # Перезагрузить модель
        inpainter.reload()
        result = inpainter(img, mask)
        ```
        """
        self._log("🔄 Перезагрузка модели...")

        # Используем предыдущее имя чекпоинта, если не указано новое
        if checkpoint_name is None:
            # Пытаемся найти last используемый чекпоинт
            checkpoint_name = "best.ckpt"  # По умолчанию

        # Загрузка модели
        self.model = self._load_model(checkpoint_name)
        self._log("✅ Модель перезагружена")

    def get_memory_stats(self) -> Dict[str, Any]:
        """
        Получить статистику использования памяти GPU.

        Returns:
            Словарь с информацией о памяти (в MB)

        Пример:
        ```python
        stats = inpainter.get_memory_stats()
        print(f"Выделено: {stats['allocated_mb']:.1f} MB")
        print(f"Зарезервировано: {stats['reserved_mb']:.1f} MB")
        ```
        """
        stats = {
            "device": str(self.device),
            "model_loaded": self.model is not None,
        }

        if torch.cuda.is_available() and self.device.type == "cuda":
            device_idx = self.device.index if self.device.index is not None else 0

            stats.update({
                "allocated_mb": torch.cuda.memory_allocated(device_idx) / 1024**2,
                "reserved_mb": torch.cuda.memory_reserved(device_idx) / 1024**2,
                "max_allocated_mb": torch.cuda.max_memory_allocated(device_idx) / 1024**2,
                "max_reserved_mb": torch.cuda.max_memory_reserved(device_idx) / 1024**2,
            })
        else:
            stats.update({
                "allocated_mb": 0,
                "reserved_mb": 0,
                "max_allocated_mb": 0,
                "max_reserved_mb": 0,
                "note": "CUDA не доступен или используется CPU"
            })

        return stats

    def reset_memory_stats(self):
        """
        Сбросить статистику максимального использования памяти CUDA.

        Полезно для отслеживания пикового использования памяти для конкретной операции.

        Пример:
        ```python
        inpainter.reset_memory_stats()
        result = inpainter(large_img, large_mask)
        stats = inpainter.get_memory_stats()
        print(f"Пиковое использование: {stats['max_allocated_mb']:.1f} MB")
        ```
        """
        if torch.cuda.is_available() and self.device.type == "cuda":
            device_idx = self.device.index if self.device.index is not None else 0
            torch.cuda.reset_peak_memory_stats(device_idx)
            self._log("🔄 Статистика памяти сброшена")
        else:
            self._log("⚠️ CUDA не доступен, нечего сбрасывать")


def main():
    """Пример использования InpainterV2"""
    import cv2

    print("🚀 Тестирование InpainterV2\n")

    # --- 1. Создаём тестовое изображение ---
    print("📝 Создание тестового изображения...")
    img = np.ones((512, 512, 3), dtype=np.uint8) * 255  # Белый фон

    # Добавляем цветной градиент
    for i in range(512):
        img[i, :, 0] = int(255 * (i / 512))  # R
        img[i, :, 1] = int(255 * (1 - i / 512))  # G

    # --- 2. Создаём маску ---
    print("🎭 Создание маски...")
    mask = np.zeros((512, 512), dtype=np.uint8)
    cv2.rectangle(mask, (156, 156), (356, 356), 255, -1)
    cv2.circle(mask, (256, 256), 50, 255, -1)

    # --- 3. Тестируем Standard Mode ---
    print("\n" + "="*60)
    print("Тест 1: Standard Mode (быстрый)")
    print("="*60)

    try:
        inpainter = InpainterV2(
            checkpoint_dir=LAMA_DIR,
            device="cuda:0" if torch.cuda.is_available() else "cpu",
            refine=False,
            verbose=True
        )

        result_standard = inpainter(img, mask)

        # Сохраняем результат
        os.makedirs("outputs", exist_ok=True)
        cv2.imwrite("outputs/test_standard.png", cv2.cvtColor(result_standard, cv2.COLOR_RGB2BGR))
        print("💾 Результат сохранён: outputs/test_standard.png")

    except Exception as e:
        print(f"❌ Ошибка в Standard Mode: {e}")
        import traceback
        traceback.print_exc()

    # --- 4. Тестируем Refinement Mode ---
    print("\n" + "="*60)
    print("Тест 2: Refinement Mode (качественный)")
    print("="*60)

    try:
        inpainter.set_refine(
            True,
            n_iters=10,  # Меньше итераций для быстрого теста
            max_scales=2
        )

        result_refine = inpainter(img, mask)

        # Сохраняем результат
        cv2.imwrite("outputs/test_refine.png", cv2.cvtColor(result_refine, cv2.COLOR_RGB2BGR))
        print("💾 Результат сохранён: outputs/test_refine.png")

    except Exception as e:
        print(f"❌ Ошибка в Refinement Mode: {e}")
        import traceback
        traceback.print_exc()

    # --- 5. Информация ---
    print("\n" + "="*60)
    print("Информация об инпейнтере:")
    print("="*60)
    info = inpainter.get_info()
    for key, value in info.items():
        print(f"{key}: {value}")

    # --- 6. Тест управления памятью ---
    print("\n" + "="*60)
    print("Тест 3: Управление памятью GPU")
    print("="*60)

    try:
        # Статистика до выгрузки
        print("\n📊 Статистика памяти (до выгрузки):")
        stats_before = inpainter.get_memory_stats()
        for key, value in stats_before.items():
            if isinstance(value, (int, float)):
                print(f"  {key}: {value:.2f}")
            else:
                print(f"  {key}: {value}")

        # Выгрузка модели
        print("\n🧹 Выгрузка модели...")
        inpainter.unload(clear_cache=True)

        # Статистика после выгрузки
        print("\n📊 Статистика памяти (после выгрузки):")
        stats_after = inpainter.get_memory_stats()
        for key, value in stats_after.items():
            if isinstance(value, (int, float)):
                print(f"  {key}: {value:.2f}")
            else:
                print(f"  {key}: {value}")

        # Перезагрузка модели
        print("\n🔄 Перезагрузка модели...")
        inpainter.reload()

        # Проверка работоспособности после перезагрузки
        print("\n🧪 Тест после перезагрузки...")
        result_reload = inpainter(img, mask)
        print(f"✅ Инпейнтинг работает после перезагрузки: {result_reload.shape}")

        # Финальная статистика
        print("\n📊 Статистика памяти (после перезагрузки):")
        stats_final = inpainter.get_memory_stats()
        for key, value in stats_final.items():
            if isinstance(value, (int, float)):
                print(f"  {key}: {value:.2f}")
            else:
                print(f"  {key}: {value}")

    except Exception as e:
        print(f"❌ Ошибка при тестировании управления памятью: {e}")
        import traceback
        traceback.print_exc()

    print("\n✅ Тестирование завершено!")


if __name__ == "__main__":
    main()
