use std::any::TypeId;
use std::borrow::Borrow;
use std::cmp::{PartialEq, PartialOrd};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::ops::{Add, BitAnd, BitOr, BitXor, Deref, Div, Mul, Neg, Rem, Shl, Shr, Sub};

use any::{Any, AnyExt};
use fn_register::{Mut, RegisterFn};
use parser::{lex, parse, Expr, FnDef, Stmt};

#[derive(Debug)]
pub enum EvalAltResult {
    ErrorFunctionNotFound,
    ErrorFunctionArgMismatch,
    ErrorFunctionCallNotSupported,
    ErrorIndexMismatch,
    ErrorIfGuardMismatch,
    ErrorVariableNotFound(String),
    ErrorFunctionArityNotSupported,
    ErrorAssignmentToUnknownLHS,
    ErrorMismatchOutputType,
    ErrorCantOpenScriptFile,
    InternalErrorMalformedDotExpression,
    LoopBreak,
    Return(Box<Any>),
}

impl Error for EvalAltResult {
    fn description(&self) -> &str {
        match *self {
            EvalAltResult::ErrorFunctionNotFound => "Function not found",
            EvalAltResult::ErrorFunctionArgMismatch => "Function argument types do not match",
            EvalAltResult::ErrorFunctionCallNotSupported => {
                "Function call with > 2 argument not supported"
            }
            EvalAltResult::ErrorIndexMismatch => "Index does not match array",
            EvalAltResult::ErrorIfGuardMismatch => "If guards expect boolean expression",
            EvalAltResult::ErrorVariableNotFound(_) => "Variable not found",
            EvalAltResult::ErrorFunctionArityNotSupported => {
                "Functions of more than 3 parameters are not yet supported"
            }
            EvalAltResult::ErrorAssignmentToUnknownLHS => {
                "Assignment to an unsupported left-hand side"
            }
            EvalAltResult::ErrorMismatchOutputType => "Cast of output failed",
            EvalAltResult::ErrorCantOpenScriptFile => "Cannot open script file",
            EvalAltResult::InternalErrorMalformedDotExpression => {
                "[Internal error] Unexpected expression in dot expression"
            }
            EvalAltResult::LoopBreak => "Loop broken before completion (not an error)",
            EvalAltResult::Return(_) => "Function returned value (not an error)",
        }
    }

    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for EvalAltResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct FnSpec {
    ident: String,
    args: Option<Vec<TypeId>>,
}

/// Rhai's engine type. This is what you use to run Rhai scripts
///
/// ```rust
/// extern crate rhai;
/// use rhai::Engine;
///
/// fn main() {
///     let mut engine = Engine::new();
///
///     if let Ok(result) = engine.eval::<i64>("40 + 2") {
///         println!("Answer: {}", result);  // prints 42
///     }
/// }
/// ```
pub struct Engine {
    /// A hashmap containing all functions know to the engine
    pub fns: HashMap<FnSpec, FnIntExt>,
}

pub enum FnIntExt {
    Ext(Box<FnAny>),
    Int(FnDef),
}

pub type FnAny = Fn(Vec<&mut Box<Any>>) -> Result<Box<Any>, EvalAltResult>;

/// A type containing information about current scope.
/// Useful for keeping state between `Engine` runs
///
/// ```rust
/// use rhai::{Engine, Scope};
///
/// let mut engine = Engine::new();
/// let mut my_scope = Scope::new();
///
/// assert!(engine.eval_with_scope::<()>(&mut my_scope, "let x = 5;").is_ok());
/// assert_eq!(engine.eval_with_scope::<i64>(&mut my_scope, "x + 1").unwrap(), 6);
/// ```
///
/// Between runs, `Engine` only remembers functions when not using own `Scope`.
pub type Scope = Vec<(String, Box<Any>)>;

impl Engine {
    /// Universal method for calling functions, that are either
    /// registered with the `Engine` or written in Rhai
    pub fn call_fn_raw(
        &self,
        ident: String,
        args: Vec<&mut Box<Any>>,
    ) -> Result<Box<Any>, EvalAltResult> {
        let spec = FnSpec {
            ident,
            args: Some(
                args.iter()
                    .map(|a| <Any as Any>::type_id(a.as_ref()))
                    .collect(),
            ),
        };

        self.fns
            .get(&spec)
            .ok_or(EvalAltResult::ErrorFunctionNotFound)
            .and_then(move |f| match *f {
                FnIntExt::Ext(ref f) => f(args),
                FnIntExt::Int(_) => unimplemented!(),
            })
    }

    pub fn register_fn_raw(&mut self, ident: String, args: Vec<TypeId>, f: Box<FnAny>) {
        let spec = FnSpec {
            ident,
            args: Some(args),
        };

        self.fns.insert(spec, FnIntExt::Ext(f));
    }

    /// Register a type for use with Engine. Keep in mind that
    /// your type must implement Clone.
    pub fn register_type<T: Any>(&mut self) {
        // currently a no-op, exists for future extensibility
    }

    /// Register a get function for a member of a registered type
    pub fn register_get<T: Clone + Any, U: Clone + Any, F>(&mut self, name: &str, get_fn: F)
    where
        F: 'static + Fn(&mut T) -> U,
    {
        let get_name = "get$".to_string() + name;
        self.register_fn(&get_name, get_fn);
    }

    /// Register a set function for a member of a registered type
    pub fn register_set<T: Clone + Any, U: Clone + Any, F>(&mut self, name: &str, set_fn: F)
    where
        F: 'static + Fn(&mut T, U) -> (),
    {
        let set_name = "set$".to_string() + name;
        self.register_fn(&set_name, set_fn);
    }

    /// Shorthand for registering both getters and setters
    pub fn register_get_set<T: Clone + Any, U: Clone + Any, F, G>(
        &mut self,
        name: &str,
        get_fn: F,
        set_fn: G,
    ) where
        F: 'static + Fn(&mut T) -> U,
        G: 'static + Fn(&mut T, U) -> (),
    {
        self.register_get(name, get_fn);
        self.register_set(name, set_fn);
    }

    fn get_dot_val_helper(
        &self,
        scope: &mut Scope,
        this_ptr: &mut Box<Any>,
        dot_rhs: &Expr,
    ) -> Result<Box<Any>, EvalAltResult> {
        use std::iter::once;

        match *dot_rhs {
            Expr::FnCall(ref fn_name, ref args) => {
                let mut args: Vec<_> = args.iter()
                    .map(|arg| self.eval_expr(scope, arg))
                    .collect::<Result<Vec<_>, _>>()?;
                let args = once(this_ptr).chain(args.iter_mut()).collect();

                self.call_fn_raw(fn_name.to_owned(), args)
            }
            Expr::Identifier(ref id) => {
                let get_fn_name = "get$".to_string() + id;

                self.call_fn_raw(get_fn_name, vec![this_ptr])
            }
            Expr::Index(ref id, ref idx_raw) => {
                let idx = self.eval_expr(scope, idx_raw)?;
                let get_fn_name = "get$".to_string() + id;

                let mut val = self.call_fn_raw(get_fn_name, vec![this_ptr])?;

                ((*val).downcast_mut() as Option<&mut Vec<Box<Any>>>)
                    .and_then(|arr| idx.downcast_ref::<i64>().map(|idx| (arr, *idx as usize)))
                    .map(|(arr, idx)| arr[idx].box_clone())
                    .ok_or(EvalAltResult::ErrorIndexMismatch)
            }
            Expr::Dot(ref inner_lhs, ref inner_rhs) => match **inner_lhs {
                Expr::Identifier(ref id) => {
                    let get_fn_name = "get$".to_string() + id;
                    self.call_fn_raw(get_fn_name, vec![this_ptr])
                        .and_then(|mut v| self.get_dot_val_helper(scope, &mut v, inner_rhs))
                }
                _ => Err(EvalAltResult::InternalErrorMalformedDotExpression),
            },
            _ => Err(EvalAltResult::InternalErrorMalformedDotExpression),
        }
    }

    fn get_dot_val(
        &self,
        scope: &mut Scope,
        dot_lhs: &Expr,
        dot_rhs: &Expr,
    ) -> Result<Box<Any>, EvalAltResult> {
        match *dot_lhs {
            Expr::Identifier(ref id) => {
                scope
                    .iter_mut()
                    .rev()
                    .find(|&&mut (ref name, _)| *id == *name)
                    .map(|&mut (_, ref mut val)| val.as_ref().box_clone())
                    .ok_or_else(|| EvalAltResult::ErrorVariableNotFound(id.clone()))
                    .and_then(|mut target| self.get_dot_val_helper(scope, &mut target, dot_rhs))

                //                let mut target: Option<Box<Any>> = None;
                //
                //                for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                //                    if *id == *name {
                //                        target = val.as_ref().box_clone();
                //                        break;
                //                    }
                //                }
                //
                //                if let Some(mut t) = target {
                //                    let result = self.get_dot_val_helper(scope, &mut t, dot_rhs);
                //
                //                    for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                //                        if *id == *name {
                //                            *val = t;
                //                            break;
                //                        }
                //                    }
                //                    return result;
                //                }
                //
                //                Err(EvalAltResult::ErrorVariableNotFound(id.clone()))
            }
            Expr::Index(ref id, ref idx_raw) => {
                let idx_boxed = self.eval_expr(scope, idx_raw)?;

                scope
                    .iter_mut()
                    .rev()
                    .find(|&&mut (ref name, _)| *id == *name)
                    .ok_or_else(|| EvalAltResult::ErrorVariableNotFound(id.clone()))
                    .and_then(|&mut (_, ref mut val)| {
                        ((*val).downcast_mut() as Option<&mut Vec<Box<Any>>>)
                            .and_then(|arr| {
                                idx_boxed
                                    .downcast_ref::<i64>()
                                    .map(|idx| arr[*idx as usize].box_clone())
                            })
                            .ok_or(EvalAltResult::ErrorIndexMismatch)
                    })
                    .and_then(|mut target| self.get_dot_val_helper(scope, &mut target, dot_rhs))

                //                let mut target: Option<Box<Any>> = None;
                //
                //                for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                //                    if *id == *name {
                //                        if let Some(arr_typed) = (*val).downcast_mut() as Option<&mut Vec<Box<Any>>>
                //                        {
                //                            let result = self.call_fn(
                //                                "clone",
                //                                Some(&mut arr_typed[*idx as usize]),
                //                                None,
                //                                None,
                //                                None,
                //                                None,
                //                                None,
                //                            );
                //
                //                            if let Ok(clone) = result {
                //                                target = Some(clone);
                //                                break;
                //                            } else {
                //                                return result;
                //                            }
                //                        } else {
                //                            return Err(EvalAltResult::ErrorIndexMismatch);
                //                        }
                //                    }
                //                }
                //
                //                if let Some(mut t) = target {
                //                    let result = self.get_dot_val_helper(scope, &mut t, dot_rhs);
                //                    for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                //                        if *id == *name {
                //                            if let Some(arr_typed) =
                //                                (*val).downcast_mut() as Option<&mut Vec<Box<Any>>>
                //                            {
                //                                arr_typed[*idx as usize] = t;
                //                                break;
                //                            }
                //                        }
                //                    }
                //                    return result;
                //                }
                //
                //                Err(EvalAltResult::ErrorVariableNotFound(id.clone()))
            }
            _ => Err(EvalAltResult::InternalErrorMalformedDotExpression),
        }
    }

    fn set_dot_val_helper(
        &self,
        this_ptr: &mut Box<Any>,
        dot_rhs: &Expr,
        mut source_val: Box<Any>,
    ) -> Result<Box<Any>, EvalAltResult> {
        match *dot_rhs {
            Expr::Identifier(ref id) => {
                let set_fn_name = "set$".to_string() + id;
                self.call_fn_raw(set_fn_name, vec![this_ptr, &mut source_val])
            }
            Expr::Dot(ref inner_lhs, ref inner_rhs) => match **inner_lhs {
                Expr::Identifier(ref id) => {
                    let get_fn_name = "get$".to_string() + id;
                    self.call_fn_raw(get_fn_name, vec![this_ptr])
                        .and_then(|mut v| self.set_dot_val_helper(&mut v, inner_rhs, source_val))
                        .and_then(|mut v| {
                            let set_fn_name = "set$".to_string() + id;

                            self.call_fn_raw(set_fn_name, vec![this_ptr, &mut v])
                        })
                }
                _ => Err(EvalAltResult::InternalErrorMalformedDotExpression),
            },
            _ => Err(EvalAltResult::InternalErrorMalformedDotExpression),
        }
    }

    fn set_dot_val(
        &self,
        scope: &mut Scope,
        dot_lhs: &Expr,
        dot_rhs: &Expr,
        source_val: Box<Any>,
    ) -> Result<Box<Any>, EvalAltResult> {
        match *dot_lhs {
            Expr::Identifier(ref id) => {
                let mut target: Option<Box<Any>> = None;

                for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                    if *id == *name {
                        target = Some(val.clone())
                    }
                }

                if let Some(mut t) = target {
                    let result = self.set_dot_val_helper(&mut t, dot_rhs, source_val);

                    for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                        if *id == *name {
                            *val = t;
                            break;
                        }
                    }
                    return result;
                }

                Err(EvalAltResult::ErrorAssignmentToUnknownLHS)
            }
            Expr::Index(ref id, ref idx_raw) => {
                let idx_boxed = self.eval_expr(scope, idx_raw)?;
                let idx = if let Some(i) = idx_boxed.downcast_ref::<i64>() {
                    *i as usize
                } else {
                    return Err(EvalAltResult::ErrorIndexMismatch);
                };

                let mut target: Option<Box<Any>> = None;

                for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                    if *id == *name {
                        if let Some(arr_typed) = (*val).downcast_mut() as Option<&mut Vec<Box<Any>>>
                        {
                            let result = Ok(arr_typed[idx].clone());

                            if let Ok(clone) = result {
                                target = Some(clone);
                                break;
                            } else {
                                return result;
                            }
                        } else {
                            return Err(EvalAltResult::ErrorIndexMismatch);
                        }
                    }
                }

                if let Some(mut t) = target {
                    let result = self.set_dot_val_helper(&mut t, dot_rhs, source_val);
                    for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                        if *id == *name {
                            if let Some(arr_typed) =
                                (*val).downcast_mut() as Option<&mut Vec<Box<Any>>>
                            {
                                arr_typed[idx] = t;
                                break;
                            }
                        }
                    }
                    return result;
                }

                Err(EvalAltResult::ErrorVariableNotFound(id.clone()))
            }
            _ => Err(EvalAltResult::InternalErrorMalformedDotExpression),
        }
    }

    fn eval_expr(&self, scope: &mut Scope, expr: &Expr) -> Result<Box<Any>, EvalAltResult> {
        match *expr {
            Expr::IntConst(i) => Ok(Box::new(i)),
            Expr::FloatConst(i) => Ok(Box::new(i)),
            Expr::StringConst(ref s) => Ok(Box::new(s.clone())),
            Expr::CharConst(ref c) => Ok(Box::new(*c)),
            Expr::Identifier(ref id) => {
                for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                    if *id == *name {
                        return Ok(val.box_clone());
                    }
                }
                Err(EvalAltResult::ErrorVariableNotFound(id.clone()))
            }
            Expr::Index(ref id, ref idx_raw) => {
                let idx = self.eval_expr(scope, idx_raw)?;

                for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                    if *id == *name {
                        if let Some(i) = idx.downcast_ref::<i64>() {
                            if let Some(arr_typed) =
                                (*val).downcast_mut() as Option<&mut Vec<Box<Any>>>
                            {
                                return Ok(arr_typed[*i as usize].box_clone());
                            } else {
                                return Err(EvalAltResult::ErrorIndexMismatch);
                            }
                        } else {
                            return Err(EvalAltResult::ErrorIndexMismatch);
                        }
                    }
                }

                Err(EvalAltResult::ErrorVariableNotFound(id.clone()))
            }
            Expr::Assignment(ref id, ref rhs) => {
                let rhs_val = self.eval_expr(scope, rhs)?;

                match **id {
                    Expr::Identifier(ref n) => {
                        for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                            if *n == *name {
                                *val = rhs_val;

                                return Ok(Box::new(()));
                            }
                        }
                        Err(EvalAltResult::ErrorVariableNotFound(n.clone()))
                    }
                    Expr::Index(ref id, ref idx_raw) => {
                        let idx = self.eval_expr(scope, idx_raw)?;

                        for &mut (ref name, ref mut val) in &mut scope.iter_mut().rev() {
                            if *id == *name {
                                if let Some(i) = idx.downcast_ref::<i64>() {
                                    if let Some(arr_typed) =
                                        (*val).downcast_mut() as Option<&mut Vec<Box<Any>>>
                                    {
                                        arr_typed[*i as usize] = rhs_val;
                                        return Ok(Box::new(()));
                                    } else {
                                        return Err(EvalAltResult::ErrorIndexMismatch);
                                    }
                                } else {
                                    return Err(EvalAltResult::ErrorIndexMismatch);
                                }
                            }
                        }

                        Err(EvalAltResult::ErrorVariableNotFound(id.clone()))
                    }
                    Expr::Dot(ref dot_lhs, ref dot_rhs) => {
                        self.set_dot_val(scope, dot_lhs, dot_rhs, rhs_val)
                    }
                    _ => Err(EvalAltResult::ErrorAssignmentToUnknownLHS),
                }
            }
            Expr::Dot(ref lhs, ref rhs) => self.get_dot_val(scope, lhs, rhs),
            Expr::Array(ref contents) => {
                let mut arr = Vec::new();

                for item in &(*contents) {
                    let arg = self.eval_expr(scope, item)?;
                    arr.push(arg);
                }

                Ok(Box::new(arr))
            }
            Expr::FnCall(ref fn_name, ref args) => self.call_fn_raw(
                fn_name.to_owned(),
                args.iter()
                    .map(|ex| self.eval_expr(scope, ex))
                    .collect::<Result<Vec<_>, _>>()?
                    .iter_mut()
                    .collect(),
            ),
            Expr::True => Ok(Box::new(true)),
            Expr::False => Ok(Box::new(false)),
        }
    }

    fn eval_stmt(&self, scope: &mut Scope, stmt: &Stmt) -> Result<Box<Any>, EvalAltResult> {
        match *stmt {
            Stmt::Expr(ref e) => self.eval_expr(scope, e),
            Stmt::Block(ref b) => {
                let prev_len = scope.len();
                let mut last_result: Result<Box<Any>, EvalAltResult> = Ok(Box::new(()));

                for s in b.iter() {
                    last_result = self.eval_stmt(scope, s);
                    if let Err(x) = last_result {
                        last_result = Err(x);
                        break;
                    }
                }

                while scope.len() > prev_len {
                    scope.pop();
                }

                last_result
            }
            Stmt::If(ref guard, ref body) => {
                let guard_result = self.eval_expr(scope, guard)?;
                match guard_result.downcast::<bool>() {
                    Ok(g) => {
                        if *g {
                            self.eval_stmt(scope, body)
                        } else {
                            Ok(Box::new(()))
                        }
                    }
                    Err(_) => Err(EvalAltResult::ErrorIfGuardMismatch),
                }
            }
            Stmt::IfElse(ref guard, ref body, ref else_body) => {
                let guard_result = self.eval_expr(scope, guard)?;
                match guard_result.downcast::<bool>() {
                    Ok(g) => {
                        if *g {
                            self.eval_stmt(scope, body)
                        } else {
                            self.eval_stmt(scope, else_body)
                        }
                    }
                    Err(_) => Err(EvalAltResult::ErrorIfGuardMismatch),
                }
            }
            Stmt::While(ref guard, ref body) => loop {
                let guard_result = self.eval_expr(scope, guard)?;
                match guard_result.downcast::<bool>() {
                    Ok(g) => {
                        if *g {
                            match self.eval_stmt(scope, body) {
                                Err(EvalAltResult::LoopBreak) => {
                                    return Ok(Box::new(()));
                                }
                                Err(x) => {
                                    return Err(x);
                                }
                                _ => (),
                            }
                        } else {
                            return Ok(Box::new(()));
                        }
                    }
                    Err(_) => return Err(EvalAltResult::ErrorIfGuardMismatch),
                }
            },
            Stmt::Loop(ref body) => loop {
                match self.eval_stmt(scope, body) {
                    Err(EvalAltResult::LoopBreak) => {
                        return Ok(Box::new(()));
                    }
                    Err(x) => {
                        return Err(x);
                    }
                    _ => (),
                }
            },
            Stmt::Break => Err(EvalAltResult::LoopBreak),
            Stmt::Return => Err(EvalAltResult::Return(Box::new(()))),
            Stmt::ReturnWithVal(ref a) => {
                let result = self.eval_expr(scope, a)?;
                Err(EvalAltResult::Return(result))
            }
            Stmt::Var(ref name, ref init) => {
                match *init {
                    Some(ref v) => {
                        let i = self.eval_expr(scope, v)?;
                        scope.push((name.clone(), i));
                    }
                    None => {
                        scope.push((name.clone(), Box::new(())));
                    }
                };
                Ok(Box::new(()))
            }
        }
    }

    /// Evaluate a file
    pub fn eval_file<T: Any + Clone>(&mut self, fname: &str) -> Result<T, EvalAltResult> {
        use std::fs::File;
        use std::io::prelude::*;

        if let Ok(mut f) = File::open(fname) {
            let mut contents = String::new();

            if f.read_to_string(&mut contents).is_ok() {
                self.eval::<T>(&contents)
            } else {
                Err(EvalAltResult::ErrorCantOpenScriptFile)
            }
        } else {
            Err(EvalAltResult::ErrorCantOpenScriptFile)
        }
    }

    /// Evaluate a string
    pub fn eval<T: Any + Clone>(&mut self, input: &str) -> Result<T, EvalAltResult> {
        let mut scope: Scope = Vec::new();

        self.eval_with_scope(&mut scope, input)
    }

    /// Evaluate with own scope
    pub fn eval_with_scope<T: Any + Clone>(
        &mut self,
        scope: &mut Scope,
        input: &str,
    ) -> Result<T, EvalAltResult> {
        let tokens = lex(input);

        let mut peekables = tokens.peekable();
        let tree = parse(&mut peekables);

        match tree {
            Ok((ref os, ref fns)) => {
                let mut x: Result<Box<Any>, EvalAltResult> = Ok(Box::new(()));

                for f in fns {
                    if f.params.len() > 6 {
                        return Err(EvalAltResult::ErrorFunctionArityNotSupported);
                    }
                    let name = f.name.clone();
                    let local_f = f.clone();

                    let spec = FnSpec {
                        ident: name,
                        args: None,
                    };

                    self.fns.insert(spec, FnIntExt::Int(local_f));
                }

                for o in os {
                    x = match self.eval_stmt(scope, o) {
                        Ok(v) => Ok(v),
                        Err(e) => return Err(e),
                    }
                }

                match x {
                    Ok(v) => match v.downcast::<T>() {
                        Ok(out) => Ok(*out),
                        Err(_) => Err(EvalAltResult::ErrorMismatchOutputType),
                    },
                    Err(e) => Err(e),
                }
            }
            Err(_) => Err(EvalAltResult::ErrorFunctionArgMismatch),
        }
    }

    /// Evaluate a file, but only return errors, if there are any.
    /// Useful for when you don't need the result, but still need
    /// to keep track of possible errors
    pub fn consume_file(&mut self, fname: &str) -> Result<(), EvalAltResult> {
        use std::fs::File;
        use std::io::prelude::*;

        if let Ok(mut f) = File::open(fname) {
            let mut contents = String::new();

            if f.read_to_string(&mut contents).is_ok() {
                if let e @ Err(_) = self.consume(&contents) {
                    return e;
                } else {
                    return Ok(());
                }
            } else {
                Err(EvalAltResult::ErrorCantOpenScriptFile)
            }
        } else {
            Err(EvalAltResult::ErrorCantOpenScriptFile)
        }
    }

    /// Evaluate a string, but only return errors, if there are any.
    /// Useful for when you don't need the result, but still need
    /// to keep track of possible errors
    pub fn consume(&mut self, input: &str) -> Result<(), EvalAltResult> {
        let mut scope: Scope = Scope::new();

        let res = self.consume_with_scope(&mut scope, input);

        res
    }

    /// Evaluate a string with own scoppe, but only return errors, if there are any.
    /// Useful for when you don't need the result, but still need
    /// to keep track of possible errors
    pub fn consume_with_scope(
        &mut self,
        scope: &mut Scope,
        input: &str,
    ) -> Result<(), EvalAltResult> {
        let tokens = lex(input);

        let mut peekables = tokens.peekable();
        let tree = parse(&mut peekables);

        match tree {
            Ok((ref os, ref fns)) => {
                for f in fns {
                    if f.params.len() > 6 {
                        return Ok(());
                    }
                    let name = f.name.clone();
                    let local_f = f.clone();

                    let spec = FnSpec {
                        ident: name,
                        args: None,
                    };

                    self.fns.insert(spec, FnIntExt::Int(local_f));
                }

                for o in os {
                    if let Err(e) = self.eval_stmt(scope, o) {
                        return Err(e);
                    }
                }

                Ok(())
            }
            Err(_) => Err(EvalAltResult::ErrorFunctionArgMismatch),
        }
    }

    /// Register the default library. That means, numberic types, char, bool
    /// String, arithmetics and string concatenations.
    pub fn register_default_lib(engine: &mut Engine) {
        engine.register_type::<i32>();
        engine.register_type::<u32>();
        engine.register_type::<i64>();
        engine.register_type::<u64>();
        engine.register_type::<f32>();
        engine.register_type::<f64>();
        engine.register_type::<String>();
        engine.register_type::<char>();
        engine.register_type::<bool>();

        macro_rules! reg_op {
            ($engine:expr, $x:expr, $op:expr, $( $y:ty ),*) => (
                $(
                    $engine.register_fn($x, ($op as fn(x: $y, y: $y)->$y));
                )*
            )
        }

        macro_rules! reg_un {
            ($engine:expr, $x:expr, $op:expr, $( $y:ty ),*) => (
                $(
                    $engine.register_fn($x, ($op as fn(x: $y)->$y));
                )*
            )
        }

        macro_rules! reg_cmp {
            ($engine:expr, $x:expr, $op:expr, $( $y:ty ),*) => (
                $(
                    $engine.register_fn($x, ($op as fn(x: $y, y: $y)->bool));
                )*
            )
        }

        fn add<T: Add>(x: T, y: T) -> <T as Add>::Output {
            x + y
        }
        fn sub<T: Sub>(x: T, y: T) -> <T as Sub>::Output {
            x - y
        }
        fn mul<T: Mul>(x: T, y: T) -> <T as Mul>::Output {
            x * y
        }
        fn div<T: Div>(x: T, y: T) -> <T as Div>::Output {
            x / y
        }
        fn neg<T: Neg>(x: T) -> <T as Neg>::Output {
            -x
        }
        fn lt<T: PartialOrd>(x: T, y: T) -> bool {
            x < y
        }
        fn lte<T: PartialOrd>(x: T, y: T) -> bool {
            x <= y
        }
        fn gt<T: PartialOrd>(x: T, y: T) -> bool {
            x > y
        }
        fn gte<T: PartialOrd>(x: T, y: T) -> bool {
            x >= y
        }
        fn eq<T: PartialEq>(x: T, y: T) -> bool {
            x == y
        }
        fn ne<T: PartialEq>(x: T, y: T) -> bool {
            x != y
        }
        fn and(x: bool, y: bool) -> bool {
            x && y
        }
        fn or(x: bool, y: bool) -> bool {
            x || y
        }
        fn not(x: bool) -> bool {
            !x
        }
        fn concat(x: String, y: String) -> String {
            x + &y
        }
        fn binary_and<T: BitAnd>(x: T, y: T) -> <T as BitAnd>::Output {
            x & y
        }
        fn binary_or<T: BitOr>(x: T, y: T) -> <T as BitOr>::Output {
            x | y
        }
        fn binary_xor<T: BitXor>(x: T, y: T) -> <T as BitXor>::Output {
            x ^ y
        }
        fn left_shift<T: Shl<T>>(x: T, y: T) -> <T as Shl<T>>::Output {
            x.shl(y)
        }
        fn right_shift<T: Shr<T>>(x: T, y: T) -> <T as Shr<T>>::Output {
            x.shr(y)
        }
        fn modulo<T: Rem<T>>(x: T, y: T) -> <T as Rem<T>>::Output {
            x % y
        }
        fn pow_i64_i64(x: i64, y: i64) -> i64 {
            x.pow(y as u32)
        }
        fn pow_f64_f64(x: f64, y: f64) -> f64 {
            x.powf(y)
        }
        fn pow_f64_i64(x: f64, y: i64) -> f64 {
            x.powi(y as i32)
        }

        reg_op!(engine, "+", add, i32, i64, u32, u64, f32, f64);
        reg_op!(engine, "-", sub, i32, i64, u32, u64, f32, f64);
        reg_op!(engine, "*", mul, i32, i64, u32, u64, f32, f64);
        reg_op!(engine, "/", div, i32, i64, u32, u64, f32, f64);

        reg_cmp!(engine, "<", lt, i32, i64, u32, u64, String, f64);
        reg_cmp!(engine, "<=", lte, i32, i64, u32, u64, String, f64);
        reg_cmp!(engine, ">", gt, i32, i64, u32, u64, String, f64);
        reg_cmp!(engine, ">=", gte, i32, i64, u32, u64, String, f64);
        reg_cmp!(engine, "==", eq, i32, i64, u32, u64, bool, String, f64);
        reg_cmp!(engine, "!=", ne, i32, i64, u32, u64, bool, String, f64);

        reg_op!(engine, "||", or, bool);
        reg_op!(engine, "&&", and, bool);
        reg_op!(engine, "|", binary_or, i32, i64, u32, u64);
        reg_op!(engine, "|", or, bool);
        reg_op!(engine, "&", binary_and, i32, i64, u32, u64);
        reg_op!(engine, "&", and, bool);
        reg_op!(engine, "^", binary_xor, i32, i64, u32, u64);
        reg_op!(engine, "<<", left_shift, i32, i64, u32, u64);
        reg_op!(engine, ">>", right_shift, i32, i64, u32, u64);
        reg_op!(engine, "%", modulo, i32, i64, u32, u64);
        engine.register_fn("~", pow_i64_i64);
        engine.register_fn("~", pow_f64_f64);
        engine.register_fn("~", pow_f64_i64);

        reg_un!(engine, "-", neg, i32, i64, f32, f64);
        reg_un!(engine, "!", not, bool);

        engine.register_fn("+", concat);

        // engine.register_fn("[]", idx);
        // FIXME?  Registering array lookups are a special case because we want to return boxes
        // directly let ent = engine.fns.entry("[]".to_string()).or_insert_with(Vec::new);
        // (*ent).push(FnType::ExternalFn2(Box::new(idx)));
    }

    /// Make a new engine
    pub fn new() -> Engine {
        let mut engine = Engine {
            fns: HashMap::new(),
        };

        // TODO
        // TODO
        // TODO
        // TODO
        //Engine::register_default_lib(&mut engine);

        engine
    }
}
