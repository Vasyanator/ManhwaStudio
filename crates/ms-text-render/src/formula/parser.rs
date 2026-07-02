/*
File: src/tabs/typing/render_next/formula/parser.rs

Purpose:
Tokenizer, AST and parser for typing formula language.

Main responsibilities:
- tokenize ASCII math expressions with numbers, identifiers and operators;
- build a small AST via recursive-descent parsing with stable precedence rules;
- expose parsed expressions for the standalone formula evaluator.

Key structures:
- `FormulaExpression`
- `FormulaNode`
- `FormulaToken` / `FormulaTokenKind`
- `FormulaParser`
*/

#[derive(Debug, Clone)]
pub(crate) struct FormulaExpression {
    pub(crate) root: FormulaNode,
}

impl FormulaExpression {
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let tokens = FormulaTokenizer::tokenize(input)?;
        let mut parser = FormulaParser::new(tokens);
        let root = parser.parse_expression()?;
        parser.expect_end()?;
        Ok(Self { root })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FormulaNode {
    Number(f32),
    Variable(String),
    Unary {
        op: FormulaUnaryOp,
        expr: Box<FormulaNode>,
    },
    Binary {
        op: FormulaBinaryOp,
        left: Box<FormulaNode>,
        right: Box<FormulaNode>,
    },
    Call {
        name: String,
        args: Vec<FormulaNode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormulaUnaryOp {
    Plus,
    Minus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormulaBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FormulaToken {
    kind: FormulaTokenKind,
    pos: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FormulaTokenKind {
    Number(f32),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    LParen,
    RParen,
    Comma,
    End,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FormulaTokenizer;

impl FormulaTokenizer {
    pub(crate) fn tokenize(input: &str) -> Result<Vec<FormulaToken>, String> {
        let bytes = input.as_bytes();
        let mut out = Vec::<FormulaToken>::new();
        let mut idx = 0usize;
        while idx < bytes.len() {
            let ch = char::from(bytes[idx]);
            if ch.is_ascii_whitespace() {
                idx += 1;
                continue;
            }
            if ch.is_ascii_digit() || ch == '.' {
                let start = idx;
                let mut has_digit = false;
                while idx < bytes.len() && char::from(bytes[idx]).is_ascii_digit() {
                    idx += 1;
                    has_digit = true;
                }
                if idx < bytes.len() && char::from(bytes[idx]) == '.' {
                    idx += 1;
                    while idx < bytes.len() && char::from(bytes[idx]).is_ascii_digit() {
                        idx += 1;
                        has_digit = true;
                    }
                }
                if !has_digit {
                    return Err(format!("invalid number at {start}"));
                }
                if idx < bytes.len() {
                    let exponent_char = char::from(bytes[idx]);
                    if exponent_char == 'e' || exponent_char == 'E' {
                        let exponent_start = idx;
                        idx += 1;
                        if idx < bytes.len() {
                            let sign = char::from(bytes[idx]);
                            if sign == '+' || sign == '-' {
                                idx += 1;
                            }
                        }
                        let exponent_digits_start = idx;
                        while idx < bytes.len() && char::from(bytes[idx]).is_ascii_digit() {
                            idx += 1;
                        }
                        if exponent_digits_start == idx {
                            return Err(format!("invalid exponent at {exponent_start}"));
                        }
                    }
                }
                let raw = &input[start..idx];
                let value = raw
                    .parse::<f32>()
                    .map_err(|_| format!("invalid float '{raw}' at {start}"))?;
                out.push(FormulaToken {
                    kind: FormulaTokenKind::Number(value),
                    pos: start,
                });
                continue;
            }
            if ch.is_ascii_alphabetic() || ch == '_' {
                let start = idx;
                idx += 1;
                while idx < bytes.len() {
                    let current = char::from(bytes[idx]);
                    if current.is_ascii_alphanumeric() || current == '_' {
                        idx += 1;
                    } else {
                        break;
                    }
                }
                out.push(FormulaToken {
                    kind: FormulaTokenKind::Ident(input[start..idx].to_ascii_lowercase()),
                    pos: start,
                });
                continue;
            }
            let kind = match ch {
                '+' => FormulaTokenKind::Plus,
                '-' => FormulaTokenKind::Minus,
                '*' => FormulaTokenKind::Star,
                '/' => FormulaTokenKind::Slash,
                '^' => FormulaTokenKind::Caret,
                '(' => FormulaTokenKind::LParen,
                ')' => FormulaTokenKind::RParen,
                ',' => FormulaTokenKind::Comma,
                _ => return Err(format!("unexpected character '{ch}' at {idx}")),
            };
            out.push(FormulaToken { kind, pos: idx });
            idx += 1;
        }
        out.push(FormulaToken {
            kind: FormulaTokenKind::End,
            pos: input.len(),
        });
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FormulaParser {
    tokens: Vec<FormulaToken>,
    idx: usize,
}

impl FormulaParser {
    pub(crate) fn new(tokens: Vec<FormulaToken>) -> Self {
        Self { tokens, idx: 0 }
    }

    pub(crate) fn parse_expression(&mut self) -> Result<FormulaNode, String> {
        self.parse_add_sub()
    }

    pub(crate) fn expect_end(&self) -> Result<(), String> {
        if matches!(self.current_kind(), FormulaTokenKind::End) {
            return Ok(());
        }
        Err(format!("unexpected token at {}", self.current_pos()))
    }

    fn parse_add_sub(&mut self) -> Result<FormulaNode, String> {
        let mut node = self.parse_mul_div()?;
        loop {
            let op = match self.current_kind() {
                FormulaTokenKind::Plus => FormulaBinaryOp::Add,
                FormulaTokenKind::Minus => FormulaBinaryOp::Sub,
                _ => break,
            };
            self.idx += 1;
            let rhs = self.parse_mul_div()?;
            node = FormulaNode::Binary {
                op,
                left: Box::new(node),
                right: Box::new(rhs),
            };
        }
        Ok(node)
    }

    fn parse_mul_div(&mut self) -> Result<FormulaNode, String> {
        let mut node = self.parse_power()?;
        loop {
            let op = match self.current_kind() {
                FormulaTokenKind::Star => FormulaBinaryOp::Mul,
                FormulaTokenKind::Slash => FormulaBinaryOp::Div,
                _ => break,
            };
            self.idx += 1;
            let rhs = self.parse_power()?;
            node = FormulaNode::Binary {
                op,
                left: Box::new(node),
                right: Box::new(rhs),
            };
        }
        Ok(node)
    }

    fn parse_power(&mut self) -> Result<FormulaNode, String> {
        let lhs = self.parse_unary()?;
        if matches!(self.current_kind(), FormulaTokenKind::Caret) {
            self.idx += 1;
            let rhs = self.parse_power()?;
            return Ok(FormulaNode::Binary {
                op: FormulaBinaryOp::Pow,
                left: Box::new(lhs),
                right: Box::new(rhs),
            });
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<FormulaNode, String> {
        match self.current_kind() {
            FormulaTokenKind::Plus => {
                self.idx += 1;
                Ok(FormulaNode::Unary {
                    op: FormulaUnaryOp::Plus,
                    expr: Box::new(self.parse_unary()?),
                })
            }
            FormulaTokenKind::Minus => {
                self.idx += 1;
                Ok(FormulaNode::Unary {
                    op: FormulaUnaryOp::Minus,
                    expr: Box::new(self.parse_unary()?),
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<FormulaNode, String> {
        match self.current_kind().clone() {
            FormulaTokenKind::Number(value) => {
                self.idx += 1;
                Ok(FormulaNode::Number(value))
            }
            FormulaTokenKind::Ident(name) => {
                self.idx += 1;
                if matches!(self.current_kind(), FormulaTokenKind::LParen) {
                    self.idx += 1;
                    let mut args = Vec::<FormulaNode>::new();
                    if !matches!(self.current_kind(), FormulaTokenKind::RParen) {
                        loop {
                            args.push(self.parse_expression()?);
                            if matches!(self.current_kind(), FormulaTokenKind::Comma) {
                                self.idx += 1;
                                continue;
                            }
                            break;
                        }
                    }
                    if !matches!(self.current_kind(), FormulaTokenKind::RParen) {
                        return Err(format!("expected ')' at {}", self.current_pos()));
                    }
                    self.idx += 1;
                    Ok(FormulaNode::Call { name, args })
                } else {
                    Ok(FormulaNode::Variable(name))
                }
            }
            FormulaTokenKind::LParen => {
                self.idx += 1;
                let inner = self.parse_expression()?;
                if !matches!(self.current_kind(), FormulaTokenKind::RParen) {
                    return Err(format!("expected ')' at {}", self.current_pos()));
                }
                self.idx += 1;
                Ok(inner)
            }
            _ => Err(format!("expected expression at {}", self.current_pos())),
        }
    }

    fn current_kind(&self) -> &FormulaTokenKind {
        &self.tokens[self.idx].kind
    }

    fn current_pos(&self) -> usize {
        self.tokens[self.idx].pos
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FormulaBinaryOp, FormulaExpression, FormulaNode, FormulaToken, FormulaTokenKind,
        FormulaTokenizer, FormulaUnaryOp,
    };

    fn token_kinds(tokens: &[FormulaToken]) -> Vec<FormulaTokenKind> {
        tokens.iter().map(|token| token.kind.clone()).collect()
    }

    #[test]
    fn tokenizer_parses_numbers_identifiers_and_exponents() {
        let tokens = match FormulaTokenizer::tokenize("Sin(.5e+1) + width_2") {
            Ok(tokens) => tokens,
            Err(error) => panic!("tokenize failed: {error}"),
        };
        assert_eq!(
            token_kinds(&tokens),
            vec![
                FormulaTokenKind::Ident("sin".to_string()),
                FormulaTokenKind::LParen,
                FormulaTokenKind::Number(5.0),
                FormulaTokenKind::RParen,
                FormulaTokenKind::Plus,
                FormulaTokenKind::Ident("width_2".to_string()),
                FormulaTokenKind::End,
            ]
        );
    }

    #[test]
    fn parser_respects_precedence_and_right_associative_power() {
        let expr = match FormulaExpression::parse("1 + 2 * 3 ^ 4 ^ 5") {
            Ok(expr) => expr,
            Err(error) => panic!("parse failed: {error}"),
        };
        assert_eq!(
            expr.root,
            FormulaNode::Binary {
                op: FormulaBinaryOp::Add,
                left: Box::new(FormulaNode::Number(1.0)),
                right: Box::new(FormulaNode::Binary {
                    op: FormulaBinaryOp::Mul,
                    left: Box::new(FormulaNode::Number(2.0)),
                    right: Box::new(FormulaNode::Binary {
                        op: FormulaBinaryOp::Pow,
                        left: Box::new(FormulaNode::Number(3.0)),
                        right: Box::new(FormulaNode::Binary {
                            op: FormulaBinaryOp::Pow,
                            left: Box::new(FormulaNode::Number(4.0)),
                            right: Box::new(FormulaNode::Number(5.0)),
                        }),
                    }),
                }),
            }
        );
    }

    #[test]
    fn parser_builds_unary_and_call_nodes() {
        let expr = match FormulaExpression::parse("-clamp(t, 0, 1)") {
            Ok(expr) => expr,
            Err(error) => panic!("parse failed: {error}"),
        };
        assert_eq!(
            expr.root,
            FormulaNode::Unary {
                op: FormulaUnaryOp::Minus,
                expr: Box::new(FormulaNode::Call {
                    name: "clamp".to_string(),
                    args: vec![
                        FormulaNode::Variable("t".to_string()),
                        FormulaNode::Number(0.0),
                        FormulaNode::Number(1.0),
                    ],
                }),
            }
        );
    }

    #[test]
    fn tokenizer_rejects_invalid_exponent() {
        let error = match FormulaTokenizer::tokenize("1e+") {
            Ok(tokens) => panic!("expected error, got tokens: {tokens:?}"),
            Err(error) => error,
        };
        assert!(error.contains("invalid exponent"));
    }

    #[test]
    fn parser_reports_missing_closing_paren() {
        let error = match FormulaExpression::parse("sin(1 + 2") {
            Ok(expr) => panic!("expected parse error, got expression: {expr:?}"),
            Err(error) => error,
        };
        assert!(error.contains("expected ')'"));
    }
}
