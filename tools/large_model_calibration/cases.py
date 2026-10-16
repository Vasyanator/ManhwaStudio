"""
File: tools/large_model_calibration/cases.py

Purpose:
The named FLUX.3 generation cases of the calibration tool: which reference images each one
uses (from `<refs-root>/edit` and `<refs-root>/obj`, the images the user picked)
and the full scene caption it sends.

Key declarations:
- `Case` - one named case (kind, references, caption)
- `CASES` - every case, in run order
- `select_cases()` - `--cases` parsing (names, kinds, `all`)
- `missing_references()`, `load_references()`

Notes:
- FLUX.3 klein has no prompt upsampler here, so every caption is a complete
  English scene caption: medium / style, scene, lighting, and each subject with
  its colours, materials and position. An EDIT caption is the caption of the
  edit image with the edit folded in; an ADD caption is the edit image's scene
  with the reference object placed in it.
- Reference 1 is always the scene (edit image); objects follow as references
  2.. in the listed order (the DiT tells them apart by the rope l axis).
- Torch-free; PIL is imported only by `load_references`.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from PIL.Image import Image

log = logging.getLogger(__name__)


class CaseKind(Enum):
    """What a case exercises: text-to-image, an edit of one image, one object
    added to a scene, or several objects added to one scene."""

    T2I = "t2i"
    EDIT = "edit"
    ADD = "add"
    MULTI = "multi"


@dataclass(frozen=True)
class Case:
    """One named generation case.

    `references` are paths relative to the refs root (`edit/...`, `obj/...`),
    in the order they are passed to the service; `caption` is sent verbatim.
    """

    name: str
    kind: CaseKind
    references: tuple[str, ...]
    caption: str
    summary: str


# ---- reference files (names as the user saved them) ----
_SFX_PANEL = "edit/убрать_звук.png"
_LANDSCAPE = "edit/landscape_1280x1024.jpg"
_KITCHEN = "edit/фото_кухни.jpg"
_GOLD_TOILET = "obj/золотой_унитаз.jpg"
_CAKE = "obj/торт.jpg"
_PAINTED_ELEPHANT = "obj/накрашенный_слон.jpeg"
_FORD_T = "obj/ford_t.jpg"

# ---- reusable scene descriptions (the edit images as they are) ----
_LANDSCAPE_SUMMER = (
    "A photorealistic wide landscape photograph of an alpine valley on a summer evening. A massive jagged "
    "grey limestone mountain wall rises in the centre and on the left, its upper faces lit warm golden-orange "
    "by the low sun, with small patches of old snow in the gullies; a second shaded grey rock face fills the "
    "right side above a small white snowfield. Dark green spruce forest climbs the lower slopes. The valley "
    "floor in the foreground is a lush bright-green meadow with scattered broad-leaved maple trees on the left "
    "and centre, a shallow stream with pale grey pebbles winding from the bottom centre toward the right, and "
    "a few small wooden farm buildings with grey roofs in the right middle distance. The sky is pale blue with "
    "a large dramatic cloud glowing fiery orange and yellow in the upper half and soft grey-brown clouds on "
    "the right. Natural colours, high detail, crisp evening light."
)

_KITCHEN_BASE = (
    "A bright, photorealistic interior photograph of a modern classic kitchen in soft natural daylight. The "
    "base cabinets and the tall cabinet column on the left, with raised-panel doors, small round knobs and a "
    "built-in black oven, are painted {cabinet_colour}. Above the counter hang white glass-front upper "
    "cabinets with stacked white plates inside. The backsplash and the floor are polished pale onyx marble "
    "with peach and grey veins; a brushed bronze faucet stands over the sink, with a wooden knife block, a "
    "paper towel roll and a slim brass wall rail along the backsplash. A black cylindrical range hood hangs on "
    "the right above the hob, beside a white plate of oranges on the cream stone counter. In the foreground "
    "stands a round white marble dining table with thin gold veins on a black pedestal base, flanked by two "
    "quilted taupe velvet armchairs with black metal legs. Four white glass pendant lamps hang from the "
    "ceiling, sheer white curtains cover a window on the right, and a strip of dark walnut wood floor runs "
    "along the bottom edge."
)

_NAVY = "deep navy blue"

_CAKE_ON_TABLE = (
    " In the centre of the round marble table stands a tall cylindrical birthday cake covered in velvety "
    "dusty-pink frosting, with gold cursive \"Happy Birthday\" lettering on its front and a gold crown set "
    "with pink teardrop jewels and clear rhinestones on top, resting on a matching pink cake board; the "
    "daylight from the window falls softly on it."
)

CASES: tuple[Case, ...] = (
    Case(
        name="t2i",
        kind=CaseKind.T2I,
        references=(),
        caption=(
            "A detailed anime illustration of a cozy manga café at dusk, seen from inside. Warm amber pendant "
            "lamps hang over rows of dark wooden tables, and tall shelves packed with colourful manga volumes "
            "line the back wall. Through a large window on the left the sky glows orange and violet above city "
            "rooftops. In the foreground a fluffy orange tabby cat sleeps curled up on a wooden chair with a red "
            "cushion, next to a table holding a white ceramic coffee cup and an open manga book. Soft warm "
            "light, gentle shadows, clean line art, rich colours."
        ),
        summary="text to image, no references",
    ),
    Case(
        name="edit_sfx",
        kind=CaseKind.EDIT,
        references=(_SFX_PANEL,),
        caption=(
            "A full-colour Korean webtoon panel in a soft painterly digital style. A small half-timbered cottage "
            "with a steep dark-grey wooden shingle roof, cream-white plaster walls framed by weathered grey "
            "beams, a narrow chimney on the left of the roof, a small lattice attic window in the gable and a "
            "little wooden barrel by the wall stands in a forest clearing. Behind it blazing red and orange "
            "autumn maple trees fill most of the frame, with a hazy green mountain and a bright blue sky in the "
            "upper right. Leafy green and yellow-green branches frame the left and right edges in the "
            "foreground, and thin rays of sunlight with glowing dust motes slant across the scene. In the lower "
            "right corner the back of a girl's head is visible: light-brown hair tied with a large coral-red "
            "ribbon bow. Clean background art with no text, no lettering, no sound effects and no motion lines."
        ),
        summary="manga cleaning: the same panel without the SFX lettering and motion marks",
    ),
    Case(
        name="edit_landscape_winter",
        kind=CaseKind.EDIT,
        references=(_LANDSCAPE,),
        caption=(
            "A photorealistic wide landscape photograph of the same alpine valley in deep winter. A massive "
            "jagged limestone mountain wall rises in the centre and on the left, its upper faces lit warm "
            "golden-orange by the low evening sun, every ledge and gully covered in fresh white snow; a second "
            "snow-covered grey rock face fills the right side. Dark spruce forest frosted with snow climbs the "
            "lower slopes. The valley floor in the foreground is a smooth blanket of untouched snow instead of "
            "grass, with bare leafless maple trees on the left and centre, a narrow partly frozen stream with "
            "grey pebbles winding from the bottom centre toward the right, and a few small wooden farm "
            "buildings with snow-laden roofs in the right middle distance. The sky is pale blue with a large "
            "dramatic cloud glowing fiery orange and yellow in the upper half and soft grey clouds on the right. "
            "Crisp cold air, high detail, natural colours."
        ),
        summary="season edit: summer meadow -> deep winter",
    ),
    Case(
        name="edit_kitchen_sage",
        kind=CaseKind.EDIT,
        references=(_KITCHEN,),
        caption=_KITCHEN_BASE.format(cabinet_colour="soft sage green with brushed brass knobs")
        + " On the table stand salt and pepper mills, a glass, a white sugar bowl and a white coffee cup on a "
        "saucer.",
        summary="colour edit: navy cabinets -> sage green",
    ),
    Case(
        name="add_cake_kitchen",
        kind=CaseKind.ADD,
        references=(_KITCHEN, _CAKE),
        caption=_KITCHEN_BASE.format(cabinet_colour=_NAVY) + _CAKE_ON_TABLE,
        summary="add one object: the pink crown cake on the kitchen table",
    ),
    Case(
        name="add_fordt_landscape",
        kind=CaseKind.ADD,
        references=(_LANDSCAPE, _FORD_T),
        caption=_LANDSCAPE_SUMMER
        + " On the green meadow just left of the stream in the right foreground stands a vintage 1920s Ford "
        "Model T touring car, angled three-quarter toward the viewer: a khaki-tan body, a folded black fabric "
        "roof, black fenders and running boards, a black radiator with round brass-rimmed headlamps and cream "
        "wooden-spoke wheels, casting a soft long evening shadow on the grass.",
        summary="add one object: the Ford Model T in the alpine meadow",
    ),
    Case(
        name="add_elephant_landscape",
        kind=CaseKind.ADD,
        references=(_LANDSCAPE, _PAINTED_ELEPHANT),
        caption=_LANDSCAPE_SUMMER
        + " In the meadow left of the stream, in the middle foreground, stands a large grey Asian elephant "
        "facing the viewer, its head, ears and trunk painted in bright festival patterns: a pink lotus flower "
        "on its forehead, yellow, blue, green and pink petals around its eyes and ears, a row of painted hearts "
        "in red, yellow, green and blue down its trunk, and a vertical floral stripe on its front leg; short "
        "curved ivory tusks, lit by the same warm evening sun.",
        summary="add one object: the painted elephant in the alpine meadow",
    ),
    Case(
        name="multi_kitchen",
        kind=CaseKind.MULTI,
        references=(_KITCHEN, _CAKE, _GOLD_TOILET),
        caption=_KITCHEN_BASE.format(cabinet_colour=_NAVY)
        + _CAKE_ON_TABLE
        + " On the marble floor in the right corner, in front of the white curtains, stands a one-piece toilet "
        "with a mirror-polished gold glaze patterned with a fine white diamond lattice and a closed glossy gold "
        "seat lid, reflecting the room.",
        summary="add two objects: the cake on the table and the golden toilet by the curtains",
    ),
)

#: The `--cases` value that selects every case.
ALL = "all"


def select_cases(spec: str) -> tuple[Case, ...]:
    """The cases `spec` names, in `CASES` order.

    `spec` is a comma list of case names, case kinds (`t2i`, `edit`, `add`,
    `multi`; a kind selects every case of that kind) or `all`. The name `t2i`
    is both a case and a kind; it selects that one case either way.

    # Raises
    `ValueError` naming every unknown token, or when nothing is selected.
    """
    tokens = [token.strip() for token in spec.split(",") if token.strip()]
    names = {case.name for case in CASES}
    kinds = {kind.value for kind in CaseKind}
    unknown = [token for token in tokens if token != ALL and token not in names and token not in kinds]
    if unknown:
        raise ValueError(
            f"unknown case(s) {unknown}; known names: {sorted(names)}, kinds: {sorted(kinds)}, or '{ALL}'"
        )
    chosen = [
        case
        for case in CASES
        if ALL in tokens or case.name in tokens or case.kind.value in tokens
    ]
    if not chosen:
        raise ValueError(f"--cases {spec!r} selects no case")
    return tuple(chosen)


def missing_references(cases: tuple[Case, ...], refs_root: Path) -> list[Path]:
    """Every reference file of `cases` that is not a file under `refs_root` (no image decode)."""
    seen: list[Path] = []
    for case in cases:
        for relative in case.references:
            path = refs_root / relative
            if not path.is_file() and path not in seen:
                seen.append(path)
    return seen


def load_references(case: Case, refs_root: Path) -> tuple[Image, ...]:
    """Decode `case`'s reference images (fully read, file closed), in order.

    The service does the EXIF transpose and RGB conversion itself.

    # Raises
    `FileNotFoundError` for a missing file; PIL's `UnidentifiedImageError` /
    `OSError` for an unreadable one.
    """
    from PIL import Image as PilImage

    images = []
    for relative in case.references:
        path = refs_root / relative
        with PilImage.open(path) as opened:
            opened.load()
            images.append(opened.copy())
        log.debug("reference %s: %dx%d %s", path, images[-1].width, images[-1].height, images[-1].mode)
    return tuple(images)
