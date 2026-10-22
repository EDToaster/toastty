//! What a compiled post shader reads, found by walking its naga module.
//!
//! The animation policy depends on it: a shader that never reads a time
//! uniform can't change between events, so it needs no time-driven
//! frames, and a shader that ignores `iMouse` needs no frame when the
//! pointer moves.

use naga::{Block, Expression, Function, Handle, Literal, Module, Statement};

/// Name of the optional event-duration constant (see the module docs of
/// [`crate::post`]).
pub(crate) const EVENT_DURATION_NAME: &str = "TOASTTY_EVENT_DURATION";

/// Bit mask (over [`super::uniforms::MEMBERS`] indexes) of the uniform
/// block members read by `entry` and every function it calls.
///
/// A member counts as read when an `AccessIndex` on the block global
/// selects it. Any other use of the block global (copying the whole
/// struct, passing it by pointer) counts as reading every member.
/// Expressions in unreachable branches still count: the result errs on
/// the side of rendering.
pub(crate) fn uniforms_read(
    module: &Module,
    block: Handle<naga::GlobalVariable>,
    entry: &str,
) -> u64 {
    let Some(ep) = module.entry_points.iter().find(|ep| ep.name == entry) else {
        return u64::MAX;
    };
    let mut used = 0u64;
    let mut visited = vec![false; module.functions.len()];
    let mut stack: Vec<&Function> = vec![&ep.function];
    let mut calls = Vec::new();
    while let Some(func) = stack.pop() {
        used |= members_read_in(func, block);
        calls.clear();
        collect_calls(&func.body, &mut calls);
        for &(callee, whole) in &calls {
            if whole_block_arg(func, block, whole) {
                return u64::MAX;
            }
            let seen = &mut visited[callee.index()];
            if !*seen {
                *seen = true;
                stack.push(&module.functions[callee]);
            }
        }
    }
    used
}

/// [`uniforms_read`] for one function body, ignoring its callees.
fn members_read_in(func: &Function, block: Handle<naga::GlobalVariable>) -> u64 {
    let mut used = 0u64;
    for (_, expr) in func.expressions.iter() {
        match *expr {
            Expression::AccessIndex { base, index } if is_block(func, block, base) => {
                used |= 1u64.checked_shl(index).unwrap_or(0);
            }
            // The whole struct copied out.
            Expression::Load { pointer } if is_block(func, block, pointer) => return u64::MAX,
            _ => {}
        }
    }
    used
}

fn is_block(func: &Function, block: Handle<naga::GlobalVariable>, h: Handle<Expression>) -> bool {
    matches!(func.expressions[h], Expression::GlobalVariable(g) if g == block)
}

/// True when one of a call's arguments is the block global itself (a
/// pointer to the whole struct).
fn whole_block_arg(
    func: &Function,
    block: Handle<naga::GlobalVariable>,
    args: &[Handle<Expression>],
) -> bool {
    args.iter().any(|&a| is_block(func, block, a))
}

/// Every `Call` in `block` and its nested blocks: the callee and the
/// call's arguments.
fn collect_calls<'a>(
    block: &'a Block,
    out: &mut Vec<(Handle<Function>, &'a [Handle<Expression>])>,
) {
    for stmt in block {
        match stmt {
            Statement::Call {
                function,
                arguments,
                ..
            } => out.push((*function, arguments)),
            Statement::Block(b) => collect_calls(b, out),
            Statement::If { accept, reject, .. } => {
                collect_calls(accept, out);
                collect_calls(reject, out);
            }
            Statement::Switch { cases, .. } => {
                for case in cases {
                    collect_calls(&case.body, out);
                }
            }
            Statement::Loop {
                body, continuing, ..
            } => {
                collect_calls(body, out);
                collect_calls(continuing, out);
            }
            _ => {}
        }
    }
}

/// Longest `TOASTTY_EVENT_DURATION` honoured (a day); larger values are
/// clamped so the schedule's `Instant` arithmetic can't overflow.
pub(crate) const MAX_EVENT_DURATION: f32 = 86_400.0;

/// The value of a module-scope `TOASTTY_EVENT_DURATION` constant, if the
/// shader declares one with a positive number (clamped to
/// [`MAX_EVENT_DURATION`]; infinity clamps too, NaN is ignored).
pub(crate) fn event_duration(module: &Module) -> Option<f32> {
    let (_, constant) = module
        .constants
        .iter()
        .find(|(_, c)| c.name.as_deref() == Some(EVENT_DURATION_NAME))?;
    #[allow(clippy::cast_precision_loss)]
    let value = match module.global_expressions[constant.init] {
        Expression::Literal(lit) => match lit {
            Literal::F32(v) => v,
            Literal::F64(v) | Literal::AbstractFloat(v) => v as f32,
            Literal::I32(v) => v as f32,
            Literal::U32(v) => v as f32,
            Literal::I64(v) | Literal::AbstractInt(v) => v as f32,
            Literal::U64(v) => v as f32,
            _ => return None,
        },
        _ => return None,
    };
    (value > 0.0).then(|| value.min(MAX_EVENT_DURATION))
}
