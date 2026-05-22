"""
File: modules/ai_device.py

Purpose:
Detect and persist the Python AI backend PyTorch device.

Main responsibilities:
- discover available PyTorch CPU/MPS/CUDA devices;
- choose an accelerated default when no user config exists;
- persist explicit device selections in UserConfig;
- provide CUDA/ROCm diagnostics for backend and Rust settings UI.

Notes:
When PyTorch is not installed, CPU is reported as a temporary runtime fallback
without writing it to config, so a later PyTorch installation can still promote
the default device to CUDA when CUDA becomes available.
"""

import os
import json
import re
import sys
import shutil
import subprocess
import platform
from dataclasses import dataclass
from typing import Any, Optional, Tuple


class CudaRocmDiagnosticsError(RuntimeError):
    """Raised when torch.cuda.is_available() is False and diagnostics find a likely cause."""


@dataclass
class _CmdResult:
    ok: bool
    out: str
    err: str
    code: int


def _run(cmd: list[str], timeout: int = 8) -> _CmdResult:
    try:
        p = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
            check=False,
        )
        return _CmdResult(p.returncode == 0, p.stdout.strip(), p.stderr.strip(), p.returncode)
    except FileNotFoundError:
        return _CmdResult(False, "", f"Command not found: {cmd[0]}", 127)
    except subprocess.TimeoutExpired:
        return _CmdResult(False, "", f"Timeout running: {' '.join(cmd)}", 124)
    except Exception as e:
        return _CmdResult(False, "", f"Error running {' '.join(cmd)}: {e}", 1)


def _is_linux() -> bool:
    return platform.system().lower() == "linux"


def _is_windows() -> bool:
    return platform.system().lower() == "windows"


def _is_macos() -> bool:
    return platform.system().lower() == "darwin"


def _which(exe: str) -> Optional[str]:
    return shutil.which(exe)


def _detect_gpu_linux() -> Tuple[bool, bool, str]:
    """
    Returns: (has_nvidia, has_amd_discrete, raw_summary)
    """
    summary_lines = []

    # Prefer lspci (most informative)
    if _which("lspci"):
        r = _run(["lspci", "-nn"])
        if r.ok and r.out:
            gpus = []
            for line in r.out.splitlines():
                if re.search(r"(VGA compatible controller|3D controller|Display controller)", line, re.I):
                    gpus.append(line)
            summary_lines.extend(gpus[:20])

            has_nvidia = any("nvidia" in l.lower() for l in gpus)
            # AMD: "Advanced Micro Devices" or "AMD/ATI"; discrete vs iGPU is hard without extra parsing.
            # We'll treat any AMD GPU listed here as "amd present"; still useful for ROCm check.
            has_amd = any(re.search(r"(amd|advanced micro devices|ati)", l, re.I) for l in gpus)
            return has_nvidia, has_amd, "\n".join(summary_lines) if summary_lines else r.out

    # Fallback: /proc/driver/nvidia/gpus exists if nvidia driver is loaded
    has_nvidia = os.path.isdir("/proc/driver/nvidia/gpus")
    has_amd = False

    # Try reading DRM cards vendor IDs
    drm_path = "/sys/class/drm"
    if os.path.isdir(drm_path):
        for d in os.listdir(drm_path):
            if not d.startswith("card"):
                continue
            vendor_file = os.path.join(drm_path, d, "device", "vendor")
            if os.path.isfile(vendor_file):
                try:
                    vendor = open(vendor_file, "r", encoding="utf-8").read().strip().lower()
                    # 0x10de NVIDIA, 0x1002 AMD
                    if vendor == "0x10de":
                        has_nvidia = True
                        summary_lines.append(f"{d}: vendor NVIDIA (0x10de)")
                    elif vendor == "0x1002":
                        has_amd = True
                        summary_lines.append(f"{d}: vendor AMD (0x1002)")
                except Exception:
                    pass

    return has_nvidia, has_amd, "\n".join(summary_lines)


def _detect_gpu_windows() -> Tuple[bool, bool, str]:
    """
    Returns: (has_nvidia, has_amd_discrete, raw_summary)
    """
    # Try wmic (legacy but often present)
    if _which("wmic"):
        r = _run(["wmic", "path", "win32_VideoController", "get", "Name,AdapterCompatibility", "/format:list"])
        if r.ok and r.out:
            out = r.out.lower()
            has_nvidia = "nvidia" in out
            has_amd = ("advanced micro devices" in out) or ("amd" in out) or ("ati" in out) or ("radeon" in out)
            return has_nvidia, has_amd, r.out

    # Try PowerShell
    if _which("powershell"):
        ps = (
            "Get-CimInstance Win32_VideoController | "
            "Select-Object Name,AdapterCompatibility | "
            "Format-List"
        )
        r = _run(["powershell", "-NoProfile", "-Command", ps])
        if r.ok and r.out:
            out = r.out.lower()
            has_nvidia = "nvidia" in out
            has_amd = ("advanced micro devices" in out) or ("amd" in out) or ("ati" in out) or ("radeon" in out)
            return has_nvidia, has_amd, r.out

    return False, False, "Не удалось получить список GPU (нет wmic/powershell или нет прав)."


def _detect_gpu_macos() -> Tuple[bool, bool, str]:
    # macOS: CUDA/ROCm обычно не применимы для PyTorch как на Linux/Windows.
    if _which("system_profiler"):
        r = _run(["system_profiler", "SPDisplaysDataType"])
        if r.ok and r.out:
            out = r.out.lower()
            has_amd = "amd" in out or "radeon" in out
            # NVIDIA на macOS может встречаться на старых системах, но CUDA в современных окружениях почти нецелесообразна
            has_nvidia = "nvidia" in out
            return has_nvidia, has_amd, r.out
    return False, False, "macOS: диагностика CUDA/ROCm ограничена (обычно используйте MPS)."


def _detect_gpu() -> Tuple[bool, bool, str]:
    if _is_linux():
        return _detect_gpu_linux()
    if _is_windows():
        return _detect_gpu_windows()
    if _is_macos():
        return _detect_gpu_macos()
    return False, False, f"Unsupported OS: {platform.system()}"


def _detect_cuda_installation() -> Tuple[bool, str]:
    """
    Returns: (cuda_present, details)
    """
    details = []

    # nvcc
    nvcc = _which("nvcc")
    if nvcc:
        r = _run([nvcc, "--version"])
        details.append(f"nvcc: {nvcc}\n{r.out or r.err}")
        if r.ok:
            return True, "\n\n".join(details)

    # nvidia-smi indicates driver availability (not toolkit, but CUDA runtime may still work)
    nvsmi = _which("nvidia-smi")
    if nvsmi:
        r = _run([nvsmi])
        details.append(f"nvidia-smi: {nvsmi}\n{r.out or r.err}")
        if r.ok:
            # Driver present; toolkit may be absent but runtime could be packaged by PyTorch wheels.
            return True, "\n\n".join(details)

    # Linux common paths
    for p in ("/usr/local/cuda", "/opt/cuda"):
        if os.path.isdir(p):
            details.append(f"CUDA directory exists: {p}")
            return True, "\n\n".join(details)

    # Windows CUDA_PATH
    cuda_path = os.environ.get("CUDA_PATH") or os.environ.get("CUDA_PATH_V12_0") or os.environ.get("CUDA_PATH_V11_8")
    if cuda_path and os.path.isdir(cuda_path):
        details.append(f"CUDA_PATH: {cuda_path}")
        return True, "\n\n".join(details)

    return False, "\n\n".join(details) if details else "CUDA toolkit/driver не обнаружены через nvcc/nvidia-smi/типовые пути."


def _detect_rocm_installation() -> Tuple[bool, str]:
    """
    Returns: (rocm_present, details)
    """
    details = []

    # rocm-smi
    rocmsmi = _which("rocm-smi")
    if rocmsmi:
        r = _run([rocmsmi, "--showproductname"])
        details.append(f"rocm-smi: {rocmsmi}\n{r.out or r.err}")
        if r.ok:
            return True, "\n\n".join(details)

    # hipcc
    hipcc = _which("hipcc")
    if hipcc:
        r = _run([hipcc, "--version"])
        details.append(f"hipcc: {hipcc}\n{r.out or r.err}")
        if r.ok:
            return True, "\n\n".join(details)

    # Linux default ROCm path
    if os.path.isdir("/opt/rocm"):
        details.append("ROCm directory exists: /opt/rocm")
        return True, "\n\n".join(details)

    # Environment hints
    for k in ("ROCM_PATH", "HIP_PATH"):
        v = os.environ.get(k)
        if v and os.path.isdir(v):
            details.append(f"{k}: {v}")
            return True, "\n\n".join(details)

    return False, "\n\n".join(details) if details else "ROCm не обнаружен через rocm-smi/hipcc/типовые пути."


def _torch_build_details() -> str:
    import torch  # type: ignore

    lines = []
    lines.append(f"torch.__version__ = {getattr(torch, '__version__', 'unknown')}")
    lines.append(f"python = {sys.version.splitlines()[0]}")
    lines.append(f"os = {platform.platform()}")
    lines.append(f"torch.version.cuda = {getattr(getattr(torch, 'version', None), 'cuda', None)}")
    lines.append(f"torch.version.hip  = {getattr(getattr(torch, 'version', None), 'hip', None)}")
    lines.append(f"torch.backends.cuda.is_built() = {getattr(getattr(torch.backends, 'cuda', None), 'is_built', lambda: None)()}")
    # Note: torch.backends.hip may not exist in all versions
    hip_backend = getattr(torch.backends, "hip", None)
    if hip_backend and hasattr(hip_backend, "is_built"):
        lines.append(f"torch.backends.hip.is_built()  = {hip_backend.is_built()}")
    lines.append(f"torch.cuda.is_available() = {torch.cuda.is_available()}")
    lines.append(f"torch.cuda.device_count() = {torch.cuda.device_count() if hasattr(torch.cuda, 'device_count') else 'n/a'}")

    # Helpful compile info if present
    try:
        cfg = torch.__config__.show()
        lines.append("\n-- torch.__config__.show() --\n" + cfg)
    except Exception as e:
        lines.append(f"torch.__config__.show() failed: {e}")

    return "\n".join(lines)


def _torch_collect_env_info() -> str:
    try:
        from torch.utils.collect_env import get_pretty_env_info  # type: ignore
        return get_pretty_env_info()
    except Exception as e:
        return f"torch.utils.collect_env.get_pretty_env_info() failed: {e}"


def _probe_torch_cuda_init_subprocess(timeout: int = 12) -> Tuple[Optional[bool], str]:
    script = (
        "import json, traceback\n"
        "res={'ok':False,'steps':[]}\n"
        "try:\n"
        " import torch\n"
        " res['steps'].append(f\"torch={torch.__version__}\")\n"
        " res['steps'].append(f\"is_available_before={torch.cuda.is_available()}\")\n"
        " torch.cuda.init()\n"
        " res['steps'].append('cuda.init=OK')\n"
        " cnt=torch.cuda.device_count()\n"
        " res['steps'].append(f\"device_count={cnt}\")\n"
        " names=[]\n"
        " for i in range(cnt):\n"
        "  try:\n"
        "   names.append(torch.cuda.get_device_name(i))\n"
        "  except Exception as e:\n"
        "   names.append(f\"<error:{e}>\")\n"
        " res['steps'].append(f\"device_names={names}\")\n"
        " res['ok']=True\n"
        "except Exception as e:\n"
        " res['steps'].append(f\"cuda.init.error={type(e).__name__}: {e}\")\n"
        " res['steps'].append(traceback.format_exc())\n"
        "print(json.dumps(res, ensure_ascii=False))\n"
    )
    r = _run([sys.executable, "-c", script], timeout=timeout)
    if not r.out:
        details = f"subprocess returned no stdout; code={r.code}\nstderr:\n{r.err or 'n/a'}"
        return None, details

    raw_line = r.out.splitlines()[-1].strip()
    try:
        payload = json.loads(raw_line)
    except Exception:
        details = (
            "failed to parse probe output as JSON.\n"
            f"exit_code={r.code}\nstdout:\n{r.out}\nstderr:\n{r.err or 'n/a'}"
        )
        return None, details

    steps = payload.get("steps")
    if not isinstance(steps, list):
        steps = [str(steps)]

    msg_lines = [f"subprocess_exit_code={r.code}", "steps:"]
    msg_lines.extend(f"- {s}" for s in steps)
    if r.err:
        msg_lines.append("stderr:")
        msg_lines.append(r.err)
    return bool(payload.get("ok")), "\n".join(msg_lines)


def _nvidia_smi_query() -> str:
    nvsmi = _which("nvidia-smi")
    if not nvsmi:
        return "nvidia-smi not found in PATH."
    r = _run(
        [
            nvsmi,
            "--query-gpu=driver_version,name,cuda_version",
            "--format=csv,noheader",
        ],
        timeout=8,
    )
    if r.ok:
        return r.out or "nvidia-smi returned empty output."
    return f"nvidia-smi query failed (code={r.code}):\n{r.err or r.out or 'n/a'}"


def _linux_cuda_device_nodes_probe() -> str:
    if not _is_linux():
        return "Skipped: device node checks are Linux-specific."

    lines = []
    static_nodes = ["/dev/nvidiactl", "/dev/nvidia-uvm", "/dev/kfd"]
    dynamic_nodes = []
    try:
        for name in os.listdir("/dev"):
            if re.fullmatch(r"nvidia\d+", name):
                dynamic_nodes.append(os.path.join("/dev", name))
    except Exception:
        pass
    paths = static_nodes + sorted(dynamic_nodes)
    if not paths:
        return "No candidate GPU device nodes detected."

    for p in paths:
        exists = os.path.exists(p)
        if not exists:
            lines.append(f"{p}: missing")
            continue
        rd = os.access(p, os.R_OK)
        wr = os.access(p, os.W_OK)
        lines.append(f"{p}: exists, readable={rd}, writable={wr}")
    return "\n".join(lines)


def _torch_relevant_env_vars() -> str:
    keys = [
        "CUDA_VISIBLE_DEVICES",
        "CUDA_DEVICE_ORDER",
        "LD_LIBRARY_PATH",
        "PATH",
        "ROCM_PATH",
        "HIP_PATH",
        "HSA_OVERRIDE_GFX_VERSION",
    ]
    lines = []
    for key in keys:
        val = os.environ.get(key)
        if val is None:
            lines.append(f"{key}=<unset>")
            continue
        if key in {"PATH", "LD_LIBRARY_PATH"} and len(val) > 400:
            lines.append(f"{key}={val[:400]}...<truncated>")
        else:
            lines.append(f"{key}={val}")
    return "\n".join(lines)


def assert_cuda_or_rocm_available() -> bool:
    """
    If torch.cuda.is_available() is True -> returns True.

    Otherwise performs diagnostics and raises CudaRocmDiagnosticsError with a detailed
    explanation and suggested next steps:
      - GPU presence (NVIDIA / AMD)
      - CUDA or ROCm presence (driver/toolkit hints)
      - whether Torch is a CUDA/ROCm build (wheel mismatch)
    """
    import torch  # type: ignore

    if torch.cuda.is_available():
        return True

    # 1) GPU presence
    has_nvidia, has_amd, gpu_details = _detect_gpu()

    # 2) CUDA/ROCm presence (system-level hints)
    cuda_present, cuda_details = _detect_cuda_installation() if has_nvidia else (False, "Пропущено: NVIDIA GPU не обнаружена.")
    rocm_present, rocm_details = _detect_rocm_installation() if has_amd and _is_linux() else (
        False,
        "Пропущено: ROCm обычно поддерживается на Linux и требует AMD GPU.",
    )
    nvidia_query = _nvidia_smi_query() if has_nvidia else "Пропущено: NVIDIA GPU не обнаружена."
    linux_nodes = _linux_cuda_device_nodes_probe()
    env_vars = _torch_relevant_env_vars()
    cuda_init_ok, cuda_init_probe = _probe_torch_cuda_init_subprocess()
    torch_collect_env = _torch_collect_env_info()

    # 3) Torch wheel / build suitability
    # torch.version.cuda is None for CPU-only builds; torch.version.hip non-None for ROCm builds.
    torch_cuda_ver = getattr(torch.version, "cuda", None)
    torch_hip_ver = getattr(torch.version, "hip", None)

    torch_is_cuda_build = torch_cuda_ver is not None
    torch_is_rocm_build = torch_hip_ver is not None

    # Craft diagnosis
    parts = []
    parts.append("torch.cuda.is_available() == False. Диагностика:\n")
    parts.append("== Torch build ==")
    parts.append(_torch_build_details())

    parts.append("\n== GPU detection ==")
    parts.append(f"NVIDIA detected: {has_nvidia}")
    parts.append(f"AMD detected:    {has_amd}")
    parts.append("GPU details:\n" + (gpu_details or "n/a"))

    parts.append("\n== CUDA detection (system) ==")
    parts.append(f"CUDA/driver/toolkit hints: {cuda_present}")
    parts.append(cuda_details or "n/a")

    parts.append("\n== ROCm detection (system) ==")
    parts.append(f"ROCm hints: {rocm_present}")
    parts.append(rocm_details or "n/a")

    parts.append("\n== nvidia-smi query ==")
    parts.append(nvidia_query)

    parts.append("\n== Linux GPU device nodes ==")
    parts.append(linux_nodes)

    parts.append("\n== torch.cuda.init() probe (subprocess) ==")
    parts.append(cuda_init_probe)

    parts.append("\n== Relevant environment variables ==")
    parts.append(env_vars)

    parts.append("\n== torch.utils.collect_env ==")
    parts.append(torch_collect_env)

    # Decide likely root cause(s)
    problems = []

    if not (has_nvidia or has_amd):
        problems.append(
            "В системе не обнаружена NVIDIA или AMD видеокарта. "
            "Если вы ожидаете дискретную GPU — проверьте, что драйверы установлены и устройство видно ОС."
        )

    # NVIDIA path
    if has_nvidia:
        if not cuda_present:
            problems.append(
                "Обнаружена NVIDIA GPU, но не найдены признаки установленного драйвера/инструментов CUDA "
                "(nvcc/nvidia-smi/типовые пути). Установите драйвер NVIDIA; для PyTorch часто достаточно драйвера, "
                "CUDA runtime может быть внутри wheel."
            )
        if not torch_is_cuda_build:
            problems.append(
                "Torch выглядит как CPU-only сборка (torch.version.cuda is None). "
                "Установите CUDA wheel PyTorch (например, cu121/cu118) подходящий под вашу систему."
            )

    # AMD / ROCm path (mostly Linux)
    if has_amd:
        if _is_linux():
            if not rocm_present:
                problems.append(
                    "Обнаружена AMD GPU, но ROCm не найден (rocm-smi/hipcc/ /opt/rocm). "
                    "Для использования AMD с PyTorch на Linux требуется ROCm-стек и совместимая GPU."
                )
            if not torch_is_rocm_build:
                # Many users install CPU or CUDA build by default.
                problems.append(
                    "Torch не выглядит как ROCm-сборка (torch.version.hip is None). "
                    "Установите PyTorch ROCm wheel, соответствующий вашей версии ROCm."
                )
        else:
            problems.append(
                "AMD GPU обнаружена, но ROCm для PyTorch обычно поддерживается на Linux. "
                "На Windows/macOS для AMD-ускорения через ROCm, как правило, не работает."
            )
    
    # --- Баг драйверов / несовместимость ---
    driver_stack_present = (
        (has_nvidia and cuda_present) or
        (has_amd and _is_linux() and rocm_present)
    )

    torch_gpu_build_present = (
        (has_nvidia and torch_is_cuda_build) or
        (has_amd and torch_is_rocm_build)
    )

    if driver_stack_present and torch_gpu_build_present and not torch.cuda.is_available():
        problems.append(
            "Баг драйверов или несовместимость низкоуровневых библиотек.\n"
            "GPU обнаружена, драйвер установлен, Torch собран с поддержкой GPU, "
            "но CUDA/ROCm недоступна.\n"
            "Возможные причины:\n"
            "- несовместимая версия драйвера и CUDA/ROCm\n"
            "- повреждённая установка драйвера\n"
            "- конфликт нескольких версий libcuda / libcudart\n"
            "- запуск в контейнере без корректного проброса GPU\n"
            "- отсутствие прав доступа к устройствам (/dev/nvidia*, /dev/kfd)\n"
        )
    # If Torch *is* CUDA build but still unavailable, likely driver/runtime mismatch
    if torch_is_cuda_build and has_nvidia and not torch.cuda.is_available():
        problems.append(
            "Torch установлен с поддержкой CUDA, но CUDA не доступна. "
            "Частые причины: отсутствует/слишком старый драйвер NVIDIA, конфликт библиотек, "
            "запуск в контейнере без проброса GPU, или несовместимость версии драйвера с CUDA, "
            "ожидаемой сборкой PyTorch."
        )

    # If Torch is ROCm build but unavailable
    if torch_is_rocm_build and has_amd and _is_linux() and not torch.cuda.is_available():
        problems.append(
            "Torch установлен с поддержкой ROCm (HIP), но устройство недоступно. "
            "Частые причины: несовместимая модель GPU, неподходящая версия ROCm, "
            "неподдерживаемое ядро/драйвер amdgpu, права на /dev/kfd, или запуск в контейнере без устройств."
        )

    if cuda_init_ok is False:
        problems.append(
            "В изолированной пробе `torch.cuda.init()` завершился ошибкой. "
            "Это подтверждает проблему инициализации CUDA на уровне драйверов/библиотек/прав доступа."
        )
    elif cuda_init_ok is None:
        problems.append(
            "Проба `torch.cuda.init()` в отдельном процессе не дала корректного результата "
            "(ошибка запуска subprocess или некорректный вывод)."
        )

    if _is_linux() and has_nvidia and "/dev/nvidiactl: missing" in linux_nodes:
        problems.append(
            "На Linux отсутствует /dev/nvidiactl при обнаруженной NVIDIA GPU. "
            "Проверьте, что драйвер NVIDIA корректно загружен."
        )

    if _is_linux() and has_amd and "/dev/kfd: missing" in linux_nodes:
        problems.append(
            "На Linux отсутствует /dev/kfd при обнаруженной AMD GPU. "
            "ROCm обычно требует доступный /dev/kfd."
        )

    cuda_visible_devices = os.environ.get("CUDA_VISIBLE_DEVICES")
    if cuda_visible_devices is not None and cuda_visible_devices.strip() == "":
        problems.append(
            "Переменная CUDA_VISIBLE_DEVICES установлена в пустое значение; это может скрывать все GPU."
        )

    # Final message
    msg = "\n".join(parts)
    if problems:
        msg += "\n\n== Вероятные проблемы ==\n- " + "\n- ".join(problems)

    # Add targeted next steps (minimal, but actionable)
    next_steps = []
    if has_nvidia:
        next_steps.append("Проверьте `nvidia-smi` (должен работать) и версию драйвера.")
        next_steps.append("Проверьте `nvidia-smi --query-gpu=driver_version,name,cuda_version --format=csv,noheader`.")
        next_steps.append("Проверьте, что установлен CUDA wheel PyTorch: `pip show torch` и `torch.version.cuda` не None.")
    if has_amd and _is_linux():
        next_steps.append("Проверьте `rocm-smi` и наличие `/opt/rocm`.")
        next_steps.append("Проверьте, что установлен ROCm wheel PyTorch: `torch.version.hip` не None.")
    if _is_linux():
        next_steps.append("Проверьте права на /dev/nvidia* и /dev/kfd.")
    next_steps.append("Проверьте CUDA_VISIBLE_DEVICES/LD_LIBRARY_PATH на конфликтующие значения.")
    if next_steps:
        msg += "\n\n== Что проверить дальше ==\n- " + "\n- ".join(next_steps)

    raise CudaRocmDiagnosticsError(msg)


class AIDevice(str):
    """
    Device wrapper that can be passed directly to torch.device(...).

    Stores selected device in UserConfig and applies changes on next app restart.
    """

    CONFIG_PATH = ("General", "ai_device")
    CONFIGURED_PATH = ("General", "ai_device_configured")

    def __new__(cls, user_config: Any):
        selected = cls._resolve_start_device(user_config)
        obj = super().__new__(cls, selected)
        obj._user_config = user_config
        return obj

    @classmethod
    def _resolve_start_device(cls, user_config: Any) -> str:
        configured = cls._get_config_value(user_config)
        available = cls.detect_available_devices()

        if cls.has_configured_device(user_config) and configured and configured in available:
            return configured

        return cls._default_device_from_available(available)

    @staticmethod
    def _default_device_from_available(available: list[str]) -> str:
        if any(dev.startswith("cuda") for dev in available):
            return "cuda"
        if "mps" in available:
            return "mps"
        return "cpu"

    @staticmethod
    def has_configured_device(user_config: Any) -> bool:
        raw_configured = AIDevice._get_raw_config_text(
            user_config,
            AIDevice.CONFIG_PATH,
        )
        if raw_configured is not None and raw_configured.strip().lower() == "not-selected":
            return False
        if AIDevice._get_bool_config_value(user_config, AIDevice.CONFIGURED_PATH):
            return True
        configured = AIDevice._get_config_value(user_config)
        return configured not in {None, "cpu"}

    @staticmethod
    def needs_manual_selection(user_config: Any, available: list[str]) -> bool:
        if AIDevice.has_configured_device(user_config):
            return False
        return AIDevice._default_device_from_available(available) != "cpu"

    @staticmethod
    def _get_config_value(user_config: Any) -> Optional[str]:
        node = AIDevice._get_raw_config_text(user_config, AIDevice.CONFIG_PATH)
        if node is None:
            return None
        text = node.strip().lower()
        if text == "not-selected":
            return None
        return node

    @staticmethod
    def _get_raw_config_text(user_config: Any, path: tuple[str, ...]) -> Optional[str]:
        node = getattr(user_config, "config", None)
        if not isinstance(node, dict):
            return None

        for key in path:
            if not isinstance(node, dict):
                return None
            node = node.get(key)

        if not isinstance(node, str):
            return None
        return node

    @staticmethod
    def _get_bool_config_value(user_config: Any, path: tuple[str, ...]) -> bool:
        node = getattr(user_config, "config", None)
        if not isinstance(node, dict):
            return False

        for key in path:
            if not isinstance(node, dict):
                return False
            node = node.get(key)

        return bool(node) if isinstance(node, bool) else False

    @staticmethod
    def _set_config_value(user_config: Any, value: str) -> None:
        node = getattr(user_config, "config", None)
        if not isinstance(node, dict):
            raise TypeError("user_config must provide dict-like 'config' attribute")

        cur = node
        for key in AIDevice.CONFIG_PATH[:-1]:
            nested = cur.get(key)
            if not isinstance(nested, dict):
                nested = {}
                cur[key] = nested
            cur = nested
        cur[AIDevice.CONFIG_PATH[-1]] = value
        cur[AIDevice.CONFIGURED_PATH[-1]] = True

        save = getattr(user_config, "save", None)
        if callable(save):
            save()

    @staticmethod
    def _normalize_device(value: str) -> str:
        val = str(value).strip().lower()
        if val in {"cpu", "mps", "cuda"}:
            return val

        if re.fullmatch(r"cuda:\d+", val):
            return val

        raise ValueError(
            "Unsupported device value. Allowed: 'cpu', 'mps', 'cuda', 'cuda:X'."
        )

    @classmethod
    def detect_available_devices(cls) -> list[str]:
        devices = ["cpu"]
        try:
            import torch  # type: ignore
        except Exception:
            return devices

        if hasattr(torch, "backends") and hasattr(torch.backends, "mps"):
            try:
                if torch.backends.mps.is_available():
                    devices.append("mps")
            except Exception:
                pass

        if hasattr(torch, "cuda"):
            try:
                if torch.cuda.is_available():
                    devices.append("cuda")
                    count = torch.cuda.device_count()
                    devices.extend(f"cuda:{idx}" for idx in range(count))
            except Exception:
                pass

        return devices

    @classmethod
    def diagnose_cuda_rocm(cls) -> str:
        try:
            assert_cuda_or_rocm_available()
            return "CUDA/ROCm доступна и torch.cuda.is_available() == True."
        except CudaRocmDiagnosticsError as exc:
            return str(exc)

    @classmethod
    def ensure_cuda_rocm_available(cls) -> bool:
        return assert_cuda_or_rocm_available()

    def change_device(self, new_device: str) -> str:
        """
        Save device to config for next app restart.

        Returns the normalized value that was saved.
        """
        normalized = self._normalize_device(new_device)
        available = self.detect_available_devices()
        if normalized not in available:
            raise ValueError(
                f"Device '{normalized}' is not available now. Available: {', '.join(available)}"
            )
        self._set_config_value(self._user_config, normalized)
        return normalized
