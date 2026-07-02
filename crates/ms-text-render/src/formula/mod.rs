/*
File: src/tabs/typing/render_next/formula/mod.rs

Purpose:
Formula-подсистема staged typing renderer.

Main responsibilities:
- отделить parser/evaluator языка формул от raster/layout кода;
- публиковать compile/eval helper-ы для следующего этапа formula render path;
- держать smoke-anchor, чтобы формульный модуль собирался до runtime-подключения.

Key modules:
- `parser.rs` — tokenizer, AST и recursive-descent parser;
- `eval.rs` — runtime variables, program bundle и curve evaluation helper-ы.
- `render.rs` — formula glyph seeds, fallback logic и rotated raster path.
*/

mod eval;
mod parser;
mod render;

pub(crate) use eval::{FormulaEvalInput, FormulaProgramBundle};
pub(crate) use render::{
    FormulaRenderOutcome, FormulaRenderRequest, render_text_with_drawn_lines_layout,
    render_text_with_formula_layout, render_text_with_vector_lines_layout,
};

pub(crate) fn touch_formula_smoke_contract() {
    let layout = crate::types::TextFormulaLayoutParams::default();
    let program = match FormulaProgramBundle::compile(&layout) {
        Ok(program) => program,
        Err(error) => panic!("render_next formula smoke contract failed to compile: {error}"),
    };
    let input = FormulaEvalInput {
        t01: 0.5,
        i: 0.0,
        n: 1.0,
        s: 0.0,
        line: 0.0,
        line_t: 0.0,
        line_n: 1.0,
        width_px: 128.0,
        font_size_px: 24.0,
        user_vars: &layout.vars,
    };
    let transform = match program.evaluate_transform_at_t01(&layout, &input, input.t01) {
        Ok(transform) => transform,
        Err(error) => panic!("render_next formula smoke contract failed to evaluate: {error}"),
    };
    let arc_samples = match program.build_arc_length_table(&layout, &input) {
        Ok(samples) => samples,
        Err(error) => panic!("render_next formula smoke contract failed to sample arc: {error}"),
    };

    std::hint::black_box((
        &layout.x_expr,
        &layout.y_expr,
        &layout.rotation_expr,
        layout.use_tangent_rotation,
        layout.t_start,
        layout.t_end,
        layout.offset_x_px,
        layout.offset_y_px,
        layout.scale_x,
        layout.scale_y,
        layout.normal_offset_px,
        layout.letter_spacing_mul,
        layout.letter_spacing_px,
        layout.vars,
    ));
    std::hint::black_box((
        input.t01,
        input.i,
        input.n,
        input.s,
        input.line,
        input.line_t,
        input.line_n,
        input.width_px,
        input.font_size_px,
        input.user_vars,
    ));
    std::hint::black_box((
        transform.center_x,
        transform.center_y,
        transform.rotation_rad,
    ));
    if let Some(last_sample) = arc_samples.last() {
        std::hint::black_box((last_sample.t01, last_sample.arc_len_px));
    }
}
