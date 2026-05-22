/*
File: src/tabs/typing/render_next/formula/eval.rs

Purpose:
Standalone evaluator and compiled program helpers for typing formula language.

Main responsibilities:
- compile layout expressions from `TextFormulaLayoutParams` into reusable programs;
- evaluate expressions against runtime variables without touching raster/layout code;
- provide curve sampling helpers for the future formula render path.

Key structures:
- `FormulaProgramBundle`
- `FormulaEvalInput`
- `FormulaVariables`
- `FormulaGlyphTransform`
*/

use super::parser::{FormulaBinaryOp, FormulaExpression, FormulaNode, FormulaUnaryOp};
use crate::tabs::typing::render_next::types::{
    TEXT_FORMULA_USER_VAR_COUNT, TextFormulaLayoutParams,
};

pub(crate) const FORMULA_ARC_LENGTH_SAMPLE_COUNT: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FormulaEvalInput<'a> {
    pub(crate) t01: f32,
    pub(crate) i: f32,
    pub(crate) n: f32,
    pub(crate) s: f32,
    pub(crate) line: f32,
    pub(crate) line_t: f32,
    pub(crate) line_n: f32,
    pub(crate) width_px: f32,
    pub(crate) font_size_px: f32,
    pub(crate) user_vars: &'a [f32; TEXT_FORMULA_USER_VAR_COUNT],
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FormulaGlyphTransform {
    pub(crate) center_x: f32,
    pub(crate) center_y: f32,
    pub(crate) rotation_rad: f32,
}

#[derive(Debug, Clone, Copy)]
struct FormulaCurvePoint {
    center_x: f32,
    center_y: f32,
    tangent_dx: f32,
    tangent_dy: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FormulaArcLengthSample {
    pub(crate) t01: f32,
    pub(crate) arc_len_px: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct FormulaProgramBundle {
    x: FormulaExpression,
    y: FormulaExpression,
    rotation: Option<FormulaExpression>,
}

impl FormulaProgramBundle {
    pub(crate) fn compile(layout: &TextFormulaLayoutParams) -> Result<Self, String> {
        let x = FormulaExpression::parse(layout.x_expr.as_str())
            .map_err(|error| format!("formula.x_expr: {error}"))?;
        let y = FormulaExpression::parse(layout.y_expr.as_str())
            .map_err(|error| format!("formula.y_expr: {error}"))?;
        let rotation = if layout.rotation_expr.trim().is_empty() {
            None
        } else {
            Some(
                FormulaExpression::parse(layout.rotation_expr.as_str())
                    .map_err(|error| format!("formula.rotation_expr: {error}"))?,
            )
        };
        Ok(Self { x, y, rotation })
    }

    pub(crate) fn evaluate_transform_at_t01(
        &self,
        layout: &TextFormulaLayoutParams,
        input: &FormulaEvalInput<'_>,
        t01: f32,
    ) -> Result<FormulaGlyphTransform, String> {
        let mapped_t = layout.t_start + t01.clamp(0.0, 1.0) * (layout.t_end - layout.t_start);
        let point = self.evaluate_curve_point(layout, input, mapped_t)?;
        let vars = FormulaVariables::new(input, mapped_t);
        let tangent_angle = point.tangent_dy.atan2(point.tangent_dx);
        let mut rotation_rad = if layout.use_tangent_rotation {
            tangent_angle
        } else {
            0.0
        };
        if let Some(rotation_expr) = self.rotation.as_ref() {
            rotation_rad += rotation_expr.eval(&vars)?;
        }
        if !point.center_x.is_finite() || !point.center_y.is_finite() || !rotation_rad.is_finite() {
            return Err("formula: вычисление дало NaN/inf".to_string());
        }
        Ok(FormulaGlyphTransform {
            center_x: point.center_x,
            center_y: point.center_y,
            rotation_rad,
        })
    }

    pub(crate) fn build_arc_length_table(
        &self,
        layout: &TextFormulaLayoutParams,
        input: &FormulaEvalInput<'_>,
    ) -> Result<Vec<FormulaArcLengthSample>, String> {
        let mut out =
            Vec::<FormulaArcLengthSample>::with_capacity(FORMULA_ARC_LENGTH_SAMPLE_COUNT + 1);
        let mut total_arc_len_px = 0.0f32;
        let mut previous_point: Option<FormulaCurvePoint> = None;
        let step_size = 1.0 / 256.0_f32;
        let mut t01 = 0.0f32;

        for step_idx in 0..=FORMULA_ARC_LENGTH_SAMPLE_COUNT {
            let sample_t01 = if step_idx == FORMULA_ARC_LENGTH_SAMPLE_COUNT {
                1.0
            } else {
                t01
            };
            let sample_input = FormulaEvalInput {
                t01: sample_t01,
                ..*input
            };
            let mapped_t = layout.t_start + sample_t01 * (layout.t_end - layout.t_start);
            let point = self.evaluate_curve_point(layout, &sample_input, mapped_t)?;
            if let Some(previous) = previous_point {
                let dx = point.center_x - previous.center_x;
                let dy = point.center_y - previous.center_y;
                let segment_len = (dx * dx + dy * dy).sqrt();
                if segment_len.is_finite() {
                    total_arc_len_px += segment_len.max(0.0);
                }
            }
            out.push(FormulaArcLengthSample {
                t01: sample_t01,
                arc_len_px: total_arc_len_px,
            });
            previous_point = Some(point);
            t01 += step_size;
        }
        Ok(out)
    }

    fn evaluate_curve_point(
        &self,
        layout: &TextFormulaLayoutParams,
        input: &FormulaEvalInput<'_>,
        mapped_t: f32,
    ) -> Result<FormulaCurvePoint, String> {
        let mut vars = FormulaVariables::new(input, mapped_t);
        let x_raw = self.x.eval(&vars)?;
        let y_raw = self.y.eval(&vars)?;
        let t_span = layout.t_end - layout.t_start;
        let epsilon = t_span.abs().max(1.0) * 1e-3;

        vars.t = mapped_t + epsilon;
        let x_plus = self.x.eval(&vars)?;
        let y_plus = self.y.eval(&vars)?;
        vars.t = mapped_t - epsilon;
        let x_minus = self.x.eval(&vars)?;
        let y_minus = self.y.eval(&vars)?;

        let mut tangent_dx = layout.scale_x * (x_plus - x_minus);
        let mut tangent_dy = layout.scale_y * (y_plus - y_minus);
        if !tangent_dx.is_finite()
            || !tangent_dy.is_finite()
            || (tangent_dx.abs() + tangent_dy.abs()) <= 1e-6
        {
            tangent_dx = 1.0;
            tangent_dy = 0.0;
        }

        let mut center_x = layout.offset_x_px + layout.scale_x * x_raw;
        let mut center_y = layout.offset_y_px + layout.scale_y * y_raw;
        if layout.normal_offset_px.abs() > f32::EPSILON {
            let tangent_len = (tangent_dx * tangent_dx + tangent_dy * tangent_dy)
                .sqrt()
                .max(1e-6);
            let tangent_x = tangent_dx / tangent_len;
            let tangent_y = tangent_dy / tangent_len;
            center_x += -tangent_y * layout.normal_offset_px;
            center_y += tangent_x * layout.normal_offset_px;
        }
        if !center_x.is_finite()
            || !center_y.is_finite()
            || !tangent_dx.is_finite()
            || !tangent_dy.is_finite()
        {
            return Err("formula: вычисление дало NaN/inf".to_string());
        }

        Ok(FormulaCurvePoint {
            center_x,
            center_y,
            tangent_dx,
            tangent_dy,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct FormulaVariables<'a> {
    t: f32,
    u: f32,
    i: f32,
    n: f32,
    s: f32,
    line: f32,
    line_t: f32,
    line_n: f32,
    width_px: f32,
    font_size_px: f32,
    user_vars: &'a [f32; TEXT_FORMULA_USER_VAR_COUNT],
}

impl<'a> FormulaVariables<'a> {
    fn new(input: &FormulaEvalInput<'a>, mapped_t: f32) -> Self {
        Self {
            t: mapped_t,
            u: input.t01 * 2.0 - 1.0,
            i: input.i,
            n: input.n,
            s: input.s,
            line: input.line,
            line_t: input.line_t,
            line_n: input.line_n,
            width_px: input.width_px,
            font_size_px: input.font_size_px,
            user_vars: input.user_vars,
        }
    }

    fn get(&self, name: &str) -> Option<f32> {
        match name {
            "t" => Some(self.t),
            "u" => Some(self.u),
            "i" => Some(self.i),
            "n" => Some(self.n),
            "s" => Some(self.s),
            "line" => Some(self.line),
            "line_t" => Some(self.line_t),
            "line_n" => Some(self.line_n),
            "w" | "width" => Some(self.width_px),
            "fs" | "font_size" => Some(self.font_size_px),
            "pi" => Some(std::f32::consts::PI),
            "tau" => Some(std::f32::consts::TAU),
            "math_e" => Some(std::f32::consts::E),
            "a" => Some(self.user_vars[0]),
            "b" => Some(self.user_vars[1]),
            "c" => Some(self.user_vars[2]),
            "d" => Some(self.user_vars[3]),
            "e" => Some(self.user_vars[4]),
            "f" => Some(self.user_vars[5]),
            "g" => Some(self.user_vars[6]),
            "h" => Some(self.user_vars[7]),
            _ => None,
        }
    }
}

impl FormulaExpression {
    fn eval(&self, vars: &FormulaVariables<'_>) -> Result<f32, String> {
        self.root.eval(vars)
    }
}

impl FormulaNode {
    fn eval(&self, vars: &FormulaVariables<'_>) -> Result<f32, String> {
        match self {
            Self::Number(value) => Ok(*value),
            Self::Variable(name) => vars
                .get(name.as_str())
                .ok_or_else(|| format!("unknown variable '{name}'")),
            Self::Unary { op, expr } => {
                let value = expr.eval(vars)?;
                let out = match op {
                    FormulaUnaryOp::Plus => value,
                    FormulaUnaryOp::Minus => -value,
                };
                ensure_formula_finite(out, "unary")
            }
            Self::Binary { op, left, right } => {
                let lhs = left.eval(vars)?;
                let rhs = right.eval(vars)?;
                let out = match op {
                    FormulaBinaryOp::Add => lhs + rhs,
                    FormulaBinaryOp::Sub => lhs - rhs,
                    FormulaBinaryOp::Mul => lhs * rhs,
                    FormulaBinaryOp::Div => lhs / rhs,
                    FormulaBinaryOp::Pow => lhs.powf(rhs),
                };
                ensure_formula_finite(out, "binary")
            }
            Self::Call { name, args } => {
                eval_formula_function(name.as_str(), args.as_slice(), vars)
            }
        }
    }
}

fn eval_formula_function(
    name: &str,
    args: &[FormulaNode],
    vars: &FormulaVariables<'_>,
) -> Result<f32, String> {
    let mut values = Vec::<f32>::with_capacity(args.len());
    for arg in args {
        values.push(arg.eval(vars)?);
    }
    let out = match name {
        "sin" => one_arg(name, &values, |value| value.sin())?,
        "cos" => one_arg(name, &values, |value| value.cos())?,
        "tan" => one_arg(name, &values, |value| value.tan())?,
        "asin" => one_arg(name, &values, |value| value.asin())?,
        "acos" => one_arg(name, &values, |value| value.acos())?,
        "atan" => one_arg(name, &values, |value| value.atan())?,
        "sqrt" => one_arg(name, &values, |value| value.sqrt())?,
        "abs" => one_arg(name, &values, |value| value.abs())?,
        "exp" => one_arg(name, &values, |value| value.exp())?,
        "ln" => one_arg(name, &values, |value| value.ln())?,
        "log" => match values.as_slice() {
            [value] => value.ln(),
            [base, value] => value.log(*base),
            _ => return Err("function log expects 1 or 2 args".to_string()),
        },
        "floor" => one_arg(name, &values, |value| value.floor())?,
        "ceil" => one_arg(name, &values, |value| value.ceil())?,
        "round" => one_arg(name, &values, |value| value.round())?,
        "sign" => one_arg(name, &values, |value| value.signum())?,
        "rad" => one_arg(name, &values, |value| value.to_radians())?,
        "deg" => one_arg(name, &values, |value| value.to_degrees())?,
        "min" => two_args(name, &values, |left, right| left.min(right))?,
        "max" => two_args(name, &values, |left, right| left.max(right))?,
        "pow" => two_args(name, &values, |left, right| left.powf(right))?,
        "atan2" => two_args(name, &values, |left, right| left.atan2(right))?,
        "clamp" => three_args(name, &values, |value, low, high| value.clamp(low, high))?,
        _ => return Err(format!("unknown function '{name}'")),
    };
    ensure_formula_finite(out, name)
}

fn one_arg(name: &str, args: &[f32], call: impl FnOnce(f32) -> f32) -> Result<f32, String> {
    match args {
        [value] => Ok(call(*value)),
        _ => Err(format!("function {name} expects 1 arg")),
    }
}

fn two_args(name: &str, args: &[f32], call: impl FnOnce(f32, f32) -> f32) -> Result<f32, String> {
    match args {
        [left, right] => Ok(call(*left, *right)),
        _ => Err(format!("function {name} expects 2 args")),
    }
}

fn three_args(
    name: &str,
    args: &[f32],
    call: impl FnOnce(f32, f32, f32) -> f32,
) -> Result<f32, String> {
    match args {
        [value, low, high] => Ok(call(*value, *low, *high)),
        _ => Err(format!("function {name} expects 3 args")),
    }
}

fn ensure_formula_finite(value: f32, label: &str) -> Result<f32, String> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("formula evaluation produced NaN/inf in {label}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{FORMULA_ARC_LENGTH_SAMPLE_COUNT, FormulaEvalInput, FormulaProgramBundle};
    use crate::tabs::typing::render_next::types::TextFormulaLayoutParams;

    fn assert_close(actual: f32, expected: f32) {
        let delta = (actual - expected).abs();
        assert!(
            delta <= 1e-3,
            "expected {expected}, got {actual}, delta {delta}"
        );
    }

    #[test]
    fn compile_reports_layout_field_name_on_parse_error() {
        let layout = TextFormulaLayoutParams {
            x_expr: "(".to_string(),
            ..TextFormulaLayoutParams::default()
        };
        let error = match FormulaProgramBundle::compile(&layout) {
            Ok(_) => panic!("expected compile error"),
            Err(error) => error,
        };
        assert!(error.contains("formula.x_expr"));
    }

    #[test]
    fn evaluate_transform_uses_layout_runtime_variables_and_rotation() {
        let layout = TextFormulaLayoutParams {
            x_expr: "t * w + a".to_string(),
            y_expr: "line_n * fs + b".to_string(),
            rotation_expr: "rad(90)".to_string(),
            t_start: 10.0,
            t_end: 14.0,
            offset_x_px: 3.0,
            offset_y_px: -2.0,
            scale_x: 2.0,
            scale_y: 0.5,
            vars: [5.0, 7.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ..TextFormulaLayoutParams::default()
        };
        let program = match FormulaProgramBundle::compile(&layout) {
            Ok(program) => program,
            Err(error) => panic!("compile failed: {error}"),
        };
        let input = FormulaEvalInput {
            t01: 0.25,
            i: 2.0,
            n: 4.0,
            s: 8.0,
            line: 1.0,
            line_t: 0.5,
            line_n: 3.0,
            width_px: 20.0,
            font_size_px: 12.0,
            user_vars: &layout.vars,
        };
        let transform = match program.evaluate_transform_at_t01(&layout, &input, input.t01) {
            Ok(transform) => transform,
            Err(error) => panic!("eval failed: {error}"),
        };
        assert_close(transform.center_x, 453.0);
        assert_close(transform.center_y, 19.5);
        assert_close(transform.rotation_rad, std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn evaluate_transform_supports_tangent_rotation_and_normal_offset() {
        let layout = TextFormulaLayoutParams {
            x_expr: "t".to_string(),
            y_expr: "2 * t".to_string(),
            use_tangent_rotation: true,
            normal_offset_px: 5.0,
            ..TextFormulaLayoutParams::default()
        };
        let program = match FormulaProgramBundle::compile(&layout) {
            Ok(program) => program,
            Err(error) => panic!("compile failed: {error}"),
        };
        let input = FormulaEvalInput {
            t01: 0.5,
            i: 0.0,
            n: 1.0,
            s: 0.0,
            line: 0.0,
            line_t: 0.0,
            line_n: 1.0,
            width_px: 100.0,
            font_size_px: 16.0,
            user_vars: &layout.vars,
        };
        let transform = match program.evaluate_transform_at_t01(&layout, &input, input.t01) {
            Ok(transform) => transform,
            Err(error) => panic!("eval failed: {error}"),
        };
        let tangent_len = 5.0_f32.sqrt();
        assert_close(transform.center_x, 0.5 - (2.0 / tangent_len) * 5.0);
        assert_close(transform.center_y, 1.0 + (1.0 / tangent_len) * 5.0);
        assert_close(transform.rotation_rad, 2.0_f32.atan2(1.0));
    }

    #[test]
    fn arc_length_table_is_monotonic_and_reaches_curve_end() {
        let layout = TextFormulaLayoutParams {
            x_expr: "t * w".to_string(),
            y_expr: "0".to_string(),
            ..TextFormulaLayoutParams::default()
        };
        let program = match FormulaProgramBundle::compile(&layout) {
            Ok(program) => program,
            Err(error) => panic!("compile failed: {error}"),
        };
        let input = FormulaEvalInput {
            t01: 0.0,
            i: 0.0,
            n: 1.0,
            s: 0.0,
            line: 0.0,
            line_t: 0.0,
            line_n: 1.0,
            width_px: 64.0,
            font_size_px: 18.0,
            user_vars: &layout.vars,
        };
        let samples = match program.build_arc_length_table(&layout, &input) {
            Ok(samples) => samples,
            Err(error) => panic!("arc sampling failed: {error}"),
        };

        assert_eq!(samples.len(), FORMULA_ARC_LENGTH_SAMPLE_COUNT + 1);
        assert_close(samples[0].t01, 0.0);
        assert_close(samples[samples.len() - 1].t01, 1.0);
        for window in samples.windows(2) {
            assert!(window[0].arc_len_px <= window[1].arc_len_px);
        }
        assert_close(samples[samples.len() - 1].arc_len_px, input.width_px);
    }

    #[test]
    fn evaluate_reports_unknown_variables() {
        let layout = TextFormulaLayoutParams {
            x_expr: "unknown_name".to_string(),
            ..TextFormulaLayoutParams::default()
        };
        let program = match FormulaProgramBundle::compile(&layout) {
            Ok(program) => program,
            Err(error) => panic!("compile failed: {error}"),
        };
        let input = FormulaEvalInput {
            t01: 0.0,
            i: 0.0,
            n: 1.0,
            s: 0.0,
            line: 0.0,
            line_t: 0.0,
            line_n: 1.0,
            width_px: 32.0,
            font_size_px: 10.0,
            user_vars: &layout.vars,
        };
        let error = match program.evaluate_transform_at_t01(&layout, &input, 0.0) {
            Ok(_) => panic!("expected evaluation error"),
            Err(error) => error,
        };
        assert!(error.contains("unknown variable"));
    }
}
