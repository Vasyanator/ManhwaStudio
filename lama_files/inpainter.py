import os
import yaml
import torch
import numpy as np
if not hasattr(np, "sctypes"):
    sctypes = {
        "int":  [np.int8, np.int16, np.int32, np.int64],
        "uint": [np.uint8, np.uint16, np.uint32, np.uint64],
        "float": [np.float16, np.float32, np.float64] + ([np.float128] if hasattr(np, "float128") else []),
        "complex": [np.complex64, np.complex128] + ([np.complex256] if hasattr(np, "complex256") else []),
        "others": [np.bool_, np.object_, np.str_],
    }
    np.sctypes = sctypes  # type: ignore[attr-defined]
import sys
from omegaconf import OmegaConf
import torch.nn.functional as F
try:
    from config import LAMA_DIR
except:
    LAMA_DIR = os.path.join(os.path.dirname(__file__), "../AI_models/Lama")
sys.path.append(os.path.dirname(__file__)) 

from saicinpainting.training.trainers import load_checkpoint
from saicinpainting.evaluation.utils import move_to_device
from saicinpainting.evaluation.refinement import refine_predict
import traceback
def _pad_to_multiple(tensor: torch.Tensor, multiple: int = 8):
    _, _, H, W = tensor.shape
    pad_h = (-(H) % multiple) % multiple
    pad_w = (-(W) % multiple) % multiple
    if pad_h == 0 and pad_w == 0:
        return tensor, None
    padded = F.pad(tensor, (0, pad_w, 0, pad_h), mode="reflect")
    return padded, (H, W)
class Inpainter:
    """
    Простая обёртка вокруг saicinpainting.

    Пример:
        inpainter = Inpainter("/path/to/checkpoint_dir")  # загружается один раз
        result = inpainter(img_rgb_uint8, mask_uint8)     # вызываем сколько угодно
    """
    def __init__(
        self,
        checkpoint_dir: str = "",
        checkpoint_name: str = "best.ckpt",   # см. что у тебя лежит в subdir models/
        device: str = "cuda:0",               # или "cpu"
        refine: bool = False,                 # включи, если хочешь two-stage inpainting
        refiner_kwargs: dict | None = None    # параметры из predict_config.refiner
    ):
        self.device = torch.device(device if (device != "cpu" and torch.cuda.is_available()) else "cpu")
        self.refine = refine
        if self.device.type == "cuda" and self.device.index is not None:
            gpu_ids_str = str(self.device.index)         # например "0"
        else:
            gpu_ids_str = ""                             # пусто = без GPU в рефайнере

        refiner_defaults = {
            "gpu_ids": gpu_ids_str,  # ВАЖНО: строка, не список!
            "modulo": 8,
            "n_iters": 20,
            "lr": 0.002,
            "min_side": 256,
            "max_scales": 5,
            "px_budget": 2_000_000,
        }
        self.refiner_kwargs = {**refiner_defaults, **(refiner_kwargs or {})}
        # --- 1. читаем config, достраиваем то, что нужно для инференса
        # cfg_path = os.path.normpath(os.path.join(checkpoint_dir, "../", "config.yaml"))
        cfg_path = os.path.join(LAMA_DIR, "config.yaml")
        with open(cfg_path, "r") as f:
            train_cfg = OmegaConf.create(yaml.safe_load(f))
        train_cfg.training_model.predict_only = True
        train_cfg.visualizer.kind = "noop"
        # --- 2. загружаем веса
        ckpt_path = os.path.join(LAMA_DIR, "models", checkpoint_name)
        self.model = load_checkpoint(train_cfg, ckpt_path, strict=False, map_location="cpu")
        self.model.freeze()
        if not self.refine:
            self.model.to(self.device)
    

    @torch.no_grad()
    def __call__(self, img: np.ndarray, mask: np.ndarray, out_key: str = "inpainted") -> np.ndarray:
        # img: H×W×3 uint8 RGB, mask: H×W uint8 (0/255)
        assert img.ndim == 3 and img.shape[2] == 3, "Ожидаю RGB-изображение"
        h, w = img.shape[:2]

        img_t = torch.from_numpy(img.astype("float32") / 255.).permute(2, 0, 1).unsqueeze(0)  # 1×3×H×W
        mask_t = torch.from_numpy((mask > 0).astype("float32")).unsqueeze(0).unsqueeze(0)     # 1×1×H×W

        # Паддим
        img_padded, _ = _pad_to_multiple(img_t, multiple=8)
        mask_padded, _ = _pad_to_multiple(mask_t, multiple=8)

        if self.refine:
            # refine_predict ожидает unpad внутри; передаём оригинальный размер в batch
            batch = {"image": img_padded, "mask": mask_padded, "unpad_to_size": (h, w)}
            res = refine_predict(batch, self.model, **self.refiner_kwargs)[0]
        else:
            batch = {"image": img_padded, "mask": mask_padded}
            batch = move_to_device(batch, self.device)
            # бинаризация маски как в оригинальном предикт-скрипте
            batch["mask"] = (batch["mask"] > 0) * 1

            batch = self.model(batch)

            if out_key not in batch:
                # fallback-логика — подставь правильно, если нужно
                for candidate in ("inpainted", "predicted_image", "output"):
                    if candidate in batch:
                        out_key = candidate
                        break

            res = batch[out_key][0]  # C×H×W

            # Если модель вернула больший размер, обрезаем по оригиналу
            unpad = batch.get("unpad_to_size", None)
            if unpad is not None:
                oh, ow = unpad
                res = res[:, :oh, :ow]
            else:
                # на всякий случай: обрежем к исходному
                res = res[:, :h, :w]

        # Приводим к numpy uint8 RGB
        res = (res.clamp(0, 1).permute(1, 2, 0).cpu().numpy() * 255).astype("uint8")
        return res

    def set_refine(self, refine: bool, **override_kwargs):
        """
        Включает/выключает режим refine и (опционально) обновляет параметры рефайнера.
        """
        # если выходим из refine в обычный режим — убедимся, что модель на устройстве
        if self.refine and not refine:
            try:
                self.model.to(self.device)
            except Exception:
                traceback.print_exc()
                pass
        self.refine = bool(refine)
        if override_kwargs:
            self.refiner_kwargs.update(override_kwargs)
if __name__ == "__main__":
    import cv2

    # --- 1. Создаём белое изображение 512×512
    img = np.ones((512, 512, 3), dtype=np.uint8) * 255

    # --- 2. Создаём квадратную маску посередине (чёрный фон, белая дыра)
    mask = np.zeros((512, 512), dtype=np.uint8)
    cv2.rectangle(mask, (156, 156), (356, 356), 255, -1)

    # --- 3. Путь к чекпоинту (замени под себя)
    checkpoint_dir = LAMA_DIR  # можно задать вручную, если config импортируется не работает
    checkpoint_name = "best.ckpt"

    # --- 4. Инициализируем Inpainter
    try:
        inpainter = Inpainter(
            checkpoint_dir=checkpoint_dir,
            checkpoint_name=checkpoint_name,
            device="cpu",      # чтобы не упираться в CUDA, можно поменять на "cuda:0"
            refine=False
        )

        # --- 5. Прогоняем картинку
        result = inpainter(img, mask)

        # --- 6. Сохраняем результат
        os.makedirs("outputs", exist_ok=True)
        out_path = os.path.join("outputs", "test_inpaint.png")
        cv2.imwrite(out_path, result)
        print(f"✅ Inpainting завершён. Результат сохранён в: {out_path}")

    except Exception as e:
        print("❌ Ошибка при запуске Inpainter:")
        traceback.print_exc()